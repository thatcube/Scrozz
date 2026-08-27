//! The settings schema.
//!
//! # Why the schema lives here and not in the store
//!
//! The schema is independent from persistence. [`crate::settings_store`] is a
//! deliberately small file-backed overlay that can be replaced by the full
//! settings/onboarding implementation without moving these public keys.
//!
//! # Naming
//!
//! `area.key-name`: a dotted area, hyphenated words. It matches the action slugs
//! the hotkey commands already use, and it survives being a TOML table, a JSON
//! object path and a command-line argument without being quoted.

use crate::{
    fault::{CliError, CliResult},
    hotkey_config::Accelerator,
    json::Json,
};

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
    /// A filesystem path. Not required to exist: a default folder on a
    /// not-yet-mounted volume is a legitimate thing to configure.
    Path,
    /// One of a fixed set of strings.
    Choice(&'static [&'static str]),
    /// A key combination, validated by the same parser the hotkey commands use.
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
    /// What it accepts.
    pub kind: Kind,
    /// The value in force when nothing has been set.
    pub default: &'static str,
    /// One line, as a GUI would show under the control.
    pub description: &'static str,
}

impl Setting {
    /// The JSON representation, including the current (always default) value.
    #[must_use]
    pub fn to_json(self) -> Json {
        self.to_json_with(self.default, "default")
    }

    /// The JSON representation with a resolved value and its source.
    #[must_use]
    pub fn to_json_with(self, value: &str, source: &str) -> Json {
        let mut fields = vec![
            ("key", Json::str(self.key)),
            ("type", Json::str(self.kind.slug())),
            ("value", Json::str(value)),
            ("default", Json::str(self.default)),
            ("source", Json::str(source)),
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
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    /// Checks a candidate value.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] describing what was expected. The message
    /// names the accepted values rather than merely saying "invalid", because a
    /// user who mistyped a setting name has no other way to discover them.
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

/// Every setting, in the order `settings get` reports them.
///
/// Grouped by area and stable: a script that diffs this output should see a
/// change only when the schema really changed.
pub const SETTINGS: &[Setting] = &[
    Setting {
        key: "capture.folder",
        kind: Kind::Path,
        default: "~/Pictures/Scrozz",
        description: "Where captures are saved when no output path is given.",
    },
    Setting {
        key: "capture.format",
        kind: Kind::Choice(&["png", "jpeg", "webp"]),
        default: "png",
        description: "Default image format.",
    },
    Setting {
        key: "capture.quality",
        kind: Kind::Int { min: 1, max: 100 },
        default: "90",
        description: "Encoder quality for lossy formats. Ignored for PNG.",
    },
    Setting {
        key: "capture.cursor",
        kind: Kind::Bool,
        default: "false",
        description: "Include the mouse pointer in captures.",
    },
    Setting {
        key: "capture.copy-to-clipboard",
        kind: Kind::Bool,
        default: "false",
        description: "Also copy every capture to the clipboard.",
    },
    Setting {
        key: "capture.window-shadow",
        kind: Kind::Bool,
        default: "true",
        description: "Include the window's drop shadow in window captures.",
    },
    Setting {
        key: "record.fps",
        kind: Kind::Int { min: 1, max: 240 },
        default: "30",
        description: "Recording frame rate.",
    },
    Setting {
        key: "record.microphone",
        kind: Kind::Bool,
        default: "false",
        description: "Record microphone input.",
    },
    Setting {
        key: "record.system-audio",
        kind: Kind::Bool,
        default: "false",
        description: "Record system audio output.",
    },
    Setting {
        key: "history.max-image-bytes",
        kind: Kind::Int {
            min: 0,
            // A little under i64::MAX so the bound is expressible in JSON
            // without a consumer needing arbitrary-precision integers.
            max: 1 << 53,
        },
        default: "10737418240",
        description: "Disk budget for stored source images. Pinned captures are never evicted.",
    },
    Setting {
        key: "system.url-scheme-enabled",
        kind: Kind::Bool,
        default: "false",
        description: "Allow registered scrozz:// links to trigger fixed, allow-listed actions.",
    },
    Setting {
        key: "hotkey.capture-region",
        kind: Kind::Accelerator,
        default: "Super+Shift+4",
        description: "Hotkey for an interactive region capture.",
    },
    Setting {
        key: "hotkey.capture-window",
        kind: Kind::Accelerator,
        default: "Super+Shift+5",
        description: "Hotkey for an interactive window capture.",
    },
    Setting {
        key: "hotkey.capture-display",
        kind: Kind::Accelerator,
        default: "Super+Shift+3",
        description: "Hotkey for capturing the active display.",
    },
    Setting {
        key: "hotkey.capture-all-displays",
        kind: Kind::Accelerator,
        default: "Super+Shift+6",
        description: "Hotkey for capturing every display.",
    },
    Setting {
        key: "hotkey.record-start",
        kind: Kind::Accelerator,
        default: "Super+Shift+R",
        description: "Hotkey for starting a recording.",
    },
    Setting {
        key: "hotkey.record-stop",
        kind: Kind::Accelerator,
        default: "Super+Shift+Escape",
        description: "Hotkey for stopping a recording.",
    },
];

/// Looks up a setting.
///
/// # Errors
///
/// Returns [`CliError::Usage`] naming the closest match, if there is one. A bare
/// "unknown key" is useless when the user is one character away.
pub fn lookup(key: &str) -> CliResult<&'static Setting> {
    if let Some(setting) = SETTINGS.iter().find(|s| s.key == key) {
        return Ok(setting);
    }
    let suggestion = closest(key);
    Err(CliError::usage(match suggestion {
        Some(near) => format!("unknown setting {key:?}; did you mean {near:?}?"),
        None => format!("unknown setting {key:?}; run `scrozz settings get` to list every key"),
    }))
}

/// The nearest known key, when one is close enough to be worth suggesting.
fn closest(key: &str) -> Option<&'static str> {
    let lower = key.to_ascii_lowercase();
    SETTINGS
        .iter()
        .map(|s| (s.key, distance(&lower, s.key)))
        // Two edits catches a typo and a missing hyphen without suggesting
        // something unrelated.
        .filter(|(_, d)| *d <= 2)
        .min_by_key(|(_, d)| *d)
        .map(|(key, _)| key)
}

/// Levenshtein distance, iterative with a single row.
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

/// Every setting as JSON.
#[must_use]
pub fn all_json() -> Json {
    Json::arr(SETTINGS.iter().copied().map(Setting::to_json))
}

/// Every setting as aligned text.
#[must_use]
pub fn all_human() -> String {
    let width = SETTINGS.iter().map(|s| s.key.len()).max().unwrap_or(0);
    SETTINGS
        .iter()
        .map(|s| format!("{:width$}  {}", s.key, s.default))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_unique() {
        let mut keys: Vec<&str> = SETTINGS.iter().map(|s| s.key).collect();
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
        // The failure this prevents is embarrassing and easy: shipping a default
        // the app itself would reject.
        for setting in SETTINGS {
            setting
                .validate(setting.default)
                .unwrap_or_else(|e| panic!("{} has an invalid default: {e}", setting.key));
        }
    }

    #[test]
    fn every_setting_has_a_description() {
        for setting in SETTINGS {
            assert!(!setting.description.is_empty(), "{}", setting.key);
            assert!(
                setting.description.ends_with('.'),
                "{} reads as a fragment",
                setting.key
            );
        }
    }

    #[test]
    fn hotkey_defaults_match_the_generated_bindings() {
        // The settings schema and the compositor config generator must not drift
        // apart; a user who changes a hotkey and regenerates their config would
        // otherwise get two different keys for one action.
        for action in crate::cli::HotkeyAction::all() {
            let key = format!("hotkey.{}", action.slug());
            let setting = lookup(&key).unwrap_or_else(|_| panic!("{key} is missing"));
            assert_eq!(
                setting.default,
                action.default_accelerator(),
                "{key} disagrees with the hotkey action"
            );
        }
    }

    #[test]
    fn the_retention_default_matches_the_core_policy() {
        // D23 fixes the default at 10 GB in `scrozz-store`. Two numbers that
        // must agree should be checked, not hoped about.
        let setting = lookup("history.max-image-bytes").unwrap();
        assert_eq!(
            setting.default.parse::<u64>().unwrap(),
            scrozz_store::RetentionPolicy::default().max_image_bytes
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
        assert!(setting.validate("0").is_err());
        assert!(setting.validate("101").is_err());
        assert!(setting.validate("-5").is_err());
        assert!(setting.validate("90.5").is_err());
        assert!(setting.validate("lots").is_err());
    }

    #[test]
    fn an_out_of_range_number_says_what_the_range_is() {
        let err = lookup("record.fps").unwrap().validate("500").unwrap_err();
        let message = err.to_string();
        assert!(message.contains('1'), "{message}");
        assert!(message.contains("240"), "{message}");
    }

    #[test]
    fn a_bad_choice_lists_the_good_ones() {
        let err = lookup("capture.format")
            .unwrap()
            .validate("gif")
            .unwrap_err();
        let message = err.to_string();
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
        // A folder on a volume that is not mounted right now is a perfectly
        // reasonable thing to configure, so existence is not checked.
        assert!(setting.validate("/Volumes/Archive/Shots").is_ok());
        assert!(setting.validate("").is_err());
        assert!(setting.validate("   ").is_err());
    }

    #[test]
    fn an_unknown_key_suggests_the_near_miss() {
        let err = lookup("capture.formats").unwrap_err();
        assert!(err.to_string().contains("capture.format"), "{err}");

        let err = lookup("capture.qualtiy").unwrap_err();
        assert!(err.to_string().contains("capture.quality"), "{err}");
    }

    #[test]
    fn a_wildly_wrong_key_points_at_the_listing_instead() {
        let err = lookup("bananas").unwrap_err();
        let message = err.to_string();
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
            .to_json()
            .to_compact_string();
        assert!(rendered.contains(r#""type":"choice""#), "{rendered}");
        assert!(
            rendered.contains(r#""choices":["png","jpeg","webp"]"#),
            "{rendered}"
        );

        let rendered = lookup("record.fps").unwrap().to_json().to_compact_string();
        assert!(rendered.contains(r#""minimum":1"#), "{rendered}");
        assert!(rendered.contains(r#""maximum":240"#), "{rendered}");
    }

    #[test]
    fn the_json_shape_starts_with_the_same_six_keys_for_every_setting() {
        for setting in SETTINGS {
            let Json::Obj(pairs) = setting.to_json() else {
                panic!("expected an object")
            };
            let keys: Vec<&str> = pairs.iter().take(6).map(|(k, _)| k.as_str()).collect();
            assert_eq!(
                keys,
                ["key", "type", "value", "default", "source", "description"],
                "{}",
                setting.key
            );
        }
    }

    #[test]
    fn values_are_reported_as_sourced_from_defaults() {
        // Until persistence exists, saying "user" would be a lie a script could
        // act on.
        for setting in SETTINGS {
            assert!(
                setting
                    .to_json()
                    .to_compact_string()
                    .contains(r#""source":"default""#),
                "{}",
                setting.key
            );
        }
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
