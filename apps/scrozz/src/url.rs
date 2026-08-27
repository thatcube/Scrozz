//! Closed mapping from external `scrozz://` URLs to trusted CLI actions.

use crate::fault::{CliError, CliResult};

const MAX_URL_BYTES: usize = 2_048;

/// An action an external URL is allowed to request.
///
/// Each variant expands to a compile-time argument vector. URL text never
/// becomes an argument, option, path, query value, or shell command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlAction {
    CaptureRegion,
    CaptureWindow,
    CaptureDisplay,
    CaptureAllDisplays,
    RecordRegion,
    RecordStop,
}

impl UrlAction {
    /// Parses one exact, canonical URL from the allow-list.
    ///
    /// # Errors
    ///
    /// Rejects unknown routes and every URL carrying parameters, fragments,
    /// credentials, escapes, control characters, or non-ASCII text.
    pub fn parse(input: &str) -> CliResult<Self> {
        if input.is_empty()
            || input.len() > MAX_URL_BYTES
            || !input.is_ascii()
            || input
                .bytes()
                .any(|byte| byte.is_ascii_control() || matches!(byte, b'?' | b'#' | b'%' | b'\\'))
        {
            return Err(rejected());
        }

        match input {
            "scrozz://capture/region" => Ok(Self::CaptureRegion),
            "scrozz://capture/window" => Ok(Self::CaptureWindow),
            "scrozz://capture/display" => Ok(Self::CaptureDisplay),
            "scrozz://capture/all-displays" => Ok(Self::CaptureAllDisplays),
            "scrozz://record/region" => Ok(Self::RecordRegion),
            "scrozz://record/stop" => Ok(Self::RecordStop),
            _ => Err(rejected()),
        }
    }

    /// Stable action name for reports.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::CaptureRegion => "capture-region",
            Self::CaptureWindow => "capture-window",
            Self::CaptureDisplay => "capture-display",
            Self::CaptureAllDisplays => "capture-all-displays",
            Self::RecordRegion => "record-region",
            Self::RecordStop => "record-stop",
        }
    }

    /// Fixed CLI arguments for this action.
    #[must_use]
    pub const fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::CaptureRegion => &["capture", "--interactive", "region"],
            Self::CaptureWindow => &["capture", "--interactive", "window"],
            Self::CaptureDisplay => &["capture", "--display", "cursor"],
            Self::CaptureAllDisplays => &["capture", "--all-displays"],
            Self::RecordRegion => &["record", "--interactive", "region"],
            Self::RecordStop => &["record", "--stop"],
        }
    }

    /// The exact canonical URL.
    #[must_use]
    pub const fn canonical_url(self) -> &'static str {
        match self {
            Self::CaptureRegion => "scrozz://capture/region",
            Self::CaptureWindow => "scrozz://capture/window",
            Self::CaptureDisplay => "scrozz://capture/display",
            Self::CaptureAllDisplays => "scrozz://capture/all-displays",
            Self::RecordRegion => "scrozz://record/region",
            Self::RecordStop => "scrozz://record/stop",
        }
    }
}

fn rejected() -> CliError {
    CliError::usage(
        "URL is not an allowed Scrozz action; URL parameters, fragments, and custom arguments are never accepted",
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const ACTIONS: [UrlAction; 6] = [
        UrlAction::CaptureRegion,
        UrlAction::CaptureWindow,
        UrlAction::CaptureDisplay,
        UrlAction::CaptureAllDisplays,
        UrlAction::RecordRegion,
        UrlAction::RecordStop,
    ];

    #[test]
    fn every_canonical_url_round_trips_to_fixed_arguments() {
        for action in ACTIONS {
            assert_eq!(UrlAction::parse(action.canonical_url()).unwrap(), action);
            assert!(!action.arguments().is_empty());
            assert!(
                action
                    .arguments()
                    .iter()
                    .all(|argument| !argument.is_empty())
            );
        }
    }

    #[test]
    fn canonical_urls_and_slugs_are_unique() {
        assert_eq!(
            ACTIONS
                .iter()
                .map(|action| action.canonical_url())
                .collect::<BTreeSet<_>>()
                .len(),
            ACTIONS.len()
        );
        assert_eq!(
            ACTIONS
                .iter()
                .map(|action| action.slug())
                .collect::<BTreeSet<_>>()
                .len(),
            ACTIONS.len()
        );
    }

    #[test]
    fn arguments_queries_fragments_escapes_and_lookalikes_are_rejected() {
        for input in [
            "scrozz://capture/region?output=/tmp/pwn",
            "scrozz://capture/region#capture",
            "scrozz://capture/%72egion",
            "scrozz://capture/region/../../update",
            "scrozz://capture/region\0",
            "SCROZZ://capture/region",
            "scrozz:////capture/region",
            "scrozz://user@capture/region",
            "scrozz://capture/region\\evil",
            "scrozz://capture/région",
            "https://capture/region",
            "scrozz://update/install",
            "scrozz://system/notify",
            "",
        ] {
            assert!(UrlAction::parse(input).is_err(), "{input:?}");
        }
    }
}
