//! The CLI's closed error type, and how each class reaches the user.
//!
//! # Why not `anyhow`
//!
//! `anyhow` is a fine type for a program whose only failure mode is "print this
//! and exit 1". This is not that program. Per decision D11 the exit status is a
//! contract (see [`crate::exit`]), which means the error type has to be *closed
//! and classifiable* — every value must map to exactly one status, and that
//! mapping must be exhaustively testable. An erased `anyhow::Error` cannot do
//! that: by the time it reaches `main`, the information the exit code depends on
//! has been thrown away.
//!
//! # Two classes of error get special treatment
//!
//! Both come straight from decisions, and both are the difference between an app
//! that feels considered and one that feels broken:
//!
//! - **[`scrozz_core::Error::PermissionDenied`] (D15).** Permission is expected
//!   on first use, not exceptional. It carries a `remedy` naming the exact
//!   settings pane in the platform's own words. It is rendered as *guidance* —
//!   never a stack trace, never a bare "error: permission denied (os error 1)".
//! - **[`scrozz_core::Error::Unsupported`] (D8).** Wayland has no window
//!   enumeration protocol and wlroots has no global-shortcut portal. These are
//!   documented gaps. The `why` field carries the reason and the alternative,
//!   and printing it is the whole remedy. A platform gap presented as a crash
//!   teaches the user the app is unreliable.
//!
//! And one that gets the opposite treatment: [`scrozz_core::Error::Cancelled`]
//! prints **nothing at all**. Its own documentation says callers must not report
//! it. Pressing Escape is a decision, not a mistake.

use std::fmt;

use scrozz_core::Error as CoreError;

use crate::{exit::Exit, json::Json};

/// Anything that can end a Scrozz invocation unsuccessfully.
#[derive(Debug)]
pub enum CliError {
    /// A failure originating in one of the Scrozz crates.
    Core(CoreError),

    /// The arguments were well-formed but semantically wrong.
    ///
    /// Reserved for objections `clap` cannot make, such as a region whose width
    /// is zero, or a `--window` pattern matching three windows at once.
    Usage(String),

    /// The capability is specified and reachable but not yet built.
    ///
    /// The workspace is deliberately contract-first (D16): most crates are trait
    /// definitions with `todo!()` bodies while they are implemented in parallel.
    /// This variant is how the CLI says so honestly instead of panicking.
    NotImplemented {
        /// What was asked for, in user terms.
        what: String,
        /// The API that will provide it, e.g. `scrozz_capture::backend`.
        provider: &'static str,
    },

    /// A running instance was reachable but the exchange failed.
    Ipc(String),
}

impl CliError {
    /// A semantic-but-not-parse objection to the arguments.
    pub fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }

    /// A capability that exists in the design but not yet in the build.
    pub fn not_implemented(what: impl Into<String>, provider: &'static str) -> Self {
        Self::NotImplemented {
            what: what.into(),
            provider,
        }
    }

    /// A failed exchange with the running instance.
    pub fn ipc(message: impl Into<String>) -> Self {
        Self::Ipc(message.into())
    }

    /// The exit status for this error.
    ///
    /// Total by construction. [`CoreError`] is `#[non_exhaustive]`, so the
    /// wildcard arm is load-bearing: a future variant must still produce a
    /// defined status rather than break the build of a downstream consumer.
    #[must_use]
    pub fn exit(&self) -> Exit {
        match self {
            Self::Usage(_) => Exit::Usage,
            Self::NotImplemented { .. } => Exit::NotImplemented,
            Self::Ipc(_) => Exit::IpcFailed,
            Self::Core(err) => match err {
                CoreError::PermissionDenied { .. } => Exit::PermissionDenied,
                CoreError::Unsupported { .. } => Exit::Unsupported,
                CoreError::TargetGone(_) => Exit::TargetGone,
                CoreError::InvalidRequest(_) => Exit::InvalidRequest,
                CoreError::Codec(_) => Exit::Codec,
                CoreError::Storage(_) => Exit::Storage,
                CoreError::Cancelled => Exit::Cancelled,
                CoreError::Io(_) => Exit::Io,
                CoreError::Platform(_) => Exit::Platform,
                _ => Exit::Failure,
            },
        }
    }

    /// The stable `kind` slug used in JSON output.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.exit().slug()
    }

    /// Whether this is ordinary user cancellation rather than a fault.
    #[must_use]
    pub fn is_cancellation(&self) -> bool {
        matches!(self, Self::Core(err) if err.is_cancellation())
    }

    /// Whether the user could plausibly fix this themselves.
    #[must_use]
    pub fn is_actionable_by_user(&self) -> bool {
        matches!(self, Self::Core(err) if err.is_actionable_by_user())
    }

    /// Variant-specific fields for the JSON `error.details` object.
    ///
    /// Kept in a nested object so the *outer* error shape never changes as new
    /// classes appear. A consumer indexes `error.kind` and `error.code`
    /// unconditionally, and reaches into `details` only for a kind it knows.
    #[must_use]
    pub fn details(&self) -> Json {
        match self {
            Self::Core(CoreError::PermissionDenied { capability, remedy }) => Json::obj([
                ("capability", Json::str(capability)),
                ("remedy", Json::str(remedy)),
            ]),
            Self::Core(CoreError::Unsupported { what, why }) => {
                Json::obj([("what", Json::str(what)), ("why", Json::str(why))])
            }
            Self::Core(CoreError::TargetGone(target)) => Json::obj([("target", Json::str(target))]),
            Self::NotImplemented { what, provider } => Json::obj([
                ("what", Json::str(what)),
                ("provider", Json::str(*provider)),
            ]),
            _ => Json::Obj(vec![]),
        }
    }

    /// The full JSON error object.
    #[must_use]
    pub fn to_json(&self) -> Json {
        Json::obj([
            ("kind", Json::str(self.kind())),
            ("code", Json::Int(i64::from(self.exit().code()))),
            ("message", Json::str(self.to_string())),
            ("cancelled", Json::Bool(self.is_cancellation())),
            ("actionable", Json::Bool(self.is_actionable_by_user())),
            ("details", self.details()),
        ])
    }

    /// The block written to stderr in human mode.
    ///
    /// Returns an empty string when nothing should be printed, which is the
    /// cancellation case and only the cancellation case.
    #[must_use]
    pub fn to_human(&self) -> String {
        match self {
            // D15's counterpart to "ask at first use": when the answer is no,
            // say what to do about it. The remedy names the pane; repeating it
            // as a bare error string would waste the one useful field we have.
            Self::Core(CoreError::PermissionDenied { capability, remedy }) => format!(
                "scrozz: {capability} access has not been granted.\n\
                 \n\
                 \x20 Grant it here:\n\
                 \x20   {remedy}\n\
                 \n\
                 \x20 Then run the command again. Scrozz asks for a permission\n\
                 \x20 only at the moment a feature needs it, so nothing else is\n\
                 \x20 waiting on you.\n"
            ),

            // D8: a documented gap, stated plainly. `why` carries the reason and
            // the alternative, so it is printed verbatim rather than summarised.
            Self::Core(CoreError::Unsupported { what, why }) => format!(
                "scrozz: {what} is not available on this system.\n\
                 \n\
                 \x20 {why}\n\
                 \n\
                 \x20 This is a known platform limitation, not a fault in Scrozz.\n"
            ),

            // Its own doc comment forbids reporting it. Silence is the feature.
            Self::Core(CoreError::Cancelled) => String::new(),

            Self::NotImplemented { what, provider } => format!(
                "scrozz: {what} is not wired up yet.\n\
                 \n\
                 \x20 The command surface is settled; the implementation behind\n\
                 \x20 `{provider}` is still landing.\n"
            ),

            Self::Usage(message) => format!(
                "scrozz: {message}\n\
                 \n\
                 \x20 Run `scrozz --help` for the full command surface.\n"
            ),

            Self::Ipc(message) => format!(
                "scrozz: the single-instance channel could not be used.\n\
                 \n\
                 \x20 {message}\n\
                 \n\
                 \x20 If a Scrozz is running but wedged, quit it from the menu bar\n\
                 \x20 or tray and try again.\n"
            ),

            other => format!("scrozz: {other}\n"),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core(err) => write!(f, "{err}"),
            Self::Usage(message) => write!(f, "{message}"),
            Self::NotImplemented { what, provider } => {
                write!(f, "{what} is not implemented yet ({provider})")
            }
            Self::Ipc(message) => write!(f, "ipc error: {message}"),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Core(err) => Some(err),
            _ => None,
        }
    }
}

impl From<CoreError> for CliError {
    fn from(value: CoreError) -> Self {
        Self::Core(value)
    }
}

impl From<std::io::Error> for CliError {
    fn from(value: std::io::Error) -> Self {
        Self::Core(CoreError::Io(value))
    }
}

/// A result carrying a [`CliError`].
pub type CliResult<T> = std::result::Result<T, CliError>;

/// Fault injection, for tests that must not touch the screen.
///
/// # Why this exists in the shipping binary
///
/// D11's third reason for the CLI is that agents cannot click. That argument
/// applies to error paths too, and error paths are the ones most likely to be
/// wrong: a permission dialog cannot be summoned on demand in CI, and a Wayland
/// gap cannot be reproduced on a Mac. Without a seam, the exit-code contract and
/// the D15 guidance rendering could only ever be tested by inspection.
///
/// Reading `SCROZZ_SIMULATE_ERROR` gives the whole pipeline — classification,
/// exit status, JSON envelope, stderr guidance — one end-to-end test per class
/// that needs no display server, no permission and no GUI.
///
/// Returns `None` when the variable is unset, and a [`CliError::Usage`] when it
/// is set to something unrecognised, so a typo in a test is loud.
pub fn simulated_error() -> Option<CliError> {
    let raw = std::env::var("SCROZZ_SIMULATE_ERROR").ok()?;
    Some(simulated_error_from(raw.trim()))
}

/// The name-to-error mapping behind [`simulated_error`], split out for testing.
#[must_use]
pub fn simulated_error_from(name: &str) -> CliError {
    match name {
        "permission-denied" => CliError::Core(CoreError::PermissionDenied {
            capability: "screen recording".into(),
            remedy: permission_remedy_hint(),
        }),
        "unsupported" => CliError::Core(CoreError::Unsupported {
            what: "window enumeration".into(),
            why: "Wayland has no protocol for listing windows. Capture a display \
                  instead; portal-owned window capture and positioned all-display \
                  composition are not yet connected to the interactive selector."
                .into(),
        }),
        "cancelled" => CliError::Core(CoreError::Cancelled),
        "target-gone" => CliError::Core(CoreError::TargetGone("window 41".into())),
        "invalid-request" => CliError::Core(CoreError::InvalidRequest("region has no area".into())),
        "codec" => CliError::Core(CoreError::Codec("encoder rejected the frame".into())),
        "storage" => CliError::Core(CoreError::Storage("history index is unreadable".into())),
        "io" => CliError::Core(CoreError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "simulated",
        ))),
        "platform" => CliError::Core(CoreError::Platform("simulated platform failure".into())),
        "not-implemented" => CliError::not_implemented("simulated capability", "scrozz_core"),
        "ipc-failed" => CliError::ipc("simulated handshake failure"),
        "usage" => CliError::usage("simulated usage objection"),
        other => CliError::usage(format!(
            "SCROZZ_SIMULATE_ERROR={other:?} is not a known error kind"
        )),
    }
}

/// The platform's own words for where screen access is granted.
///
/// Per D15 the remedy has to name the real pane. "Check your settings" is the
/// unhelpful default this exists to avoid.
#[must_use]
pub fn permission_remedy_hint() -> String {
    if cfg!(target_os = "macos") {
        "System Settings → Privacy & Security → Screen & System Audio Recording".into()
    } else if cfg!(target_os = "windows") {
        "Settings → Privacy & security → App permissions".into()
    } else {
        "your desktop's screen-sharing permissions (xdg-desktop-portal)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One representative value of every [`CoreError`] variant, so the mapping
    /// tests below cannot silently skip a class.
    fn every_core_error() -> Vec<CoreError> {
        vec![
            CoreError::PermissionDenied {
                capability: "screen recording".into(),
                remedy: "System Settings → Privacy & Security".into(),
            },
            CoreError::Unsupported {
                what: "window enumeration".into(),
                why: "no Wayland protocol; capture a display instead".into(),
            },
            CoreError::TargetGone("window 7".into()),
            CoreError::InvalidRequest("zero area".into()),
            CoreError::Codec("bad frame".into()),
            CoreError::Storage("index locked".into()),
            CoreError::Cancelled,
            CoreError::Io(std::io::Error::other("disk")),
            CoreError::Platform("CGError 1001".into()),
        ]
    }

    #[test]
    fn every_core_variant_maps_to_its_own_exit_status() {
        let expected = [
            Exit::PermissionDenied,
            Exit::Unsupported,
            Exit::TargetGone,
            Exit::InvalidRequest,
            Exit::Codec,
            Exit::Storage,
            Exit::Cancelled,
            Exit::Io,
            Exit::Platform,
        ];
        for (err, want) in every_core_error().into_iter().zip(expected) {
            assert_eq!(CliError::Core(err).exit(), want);
        }
    }

    #[test]
    fn cli_only_variants_map_to_their_own_statuses() {
        assert_eq!(CliError::usage("x").exit(), Exit::Usage);
        assert_eq!(
            CliError::not_implemented("x", "y").exit(),
            Exit::NotImplemented
        );
        assert_eq!(CliError::ipc("x").exit(), Exit::IpcFailed);
    }

    #[test]
    fn cancellation_is_the_only_non_fault_error() {
        for err in every_core_error() {
            let cancelled = err.is_cancellation();
            let cli = CliError::Core(err);
            assert_eq!(cli.exit().is_fault(), !cancelled);
        }
    }

    #[test]
    fn cancellation_prints_nothing() {
        // Its doc comment: "callers must not report it to the user".
        let err = CliError::Core(CoreError::Cancelled);
        assert!(err.to_human().is_empty());
        assert_eq!(err.exit(), Exit::Cancelled);
        assert!(err.is_cancellation());
    }

    #[test]
    fn every_other_error_prints_something() {
        for err in every_core_error() {
            let cancelled = err.is_cancellation();
            let cli = CliError::Core(err);
            assert_eq!(cli.to_human().is_empty(), cancelled);
        }
    }

    #[test]
    fn permission_denied_prints_the_remedy_and_no_debug_noise() {
        let err = CliError::Core(CoreError::PermissionDenied {
            capability: "screen recording".into(),
            remedy: "System Settings → Privacy & Security → Screen Recording".into(),
        });
        let text = err.to_human();
        assert!(text.contains("System Settings → Privacy & Security → Screen Recording"));
        assert!(text.contains("Grant it here"));
        // D15: guidance, never a stack trace or a debug dump.
        assert!(!text.contains("PermissionDenied"));
        assert!(!text.contains("panicked"));
        assert!(!text.contains("RUST_BACKTRACE"));
        assert!(err.is_actionable_by_user());
    }

    #[test]
    fn unsupported_prints_the_reason_and_the_alternative_verbatim() {
        let why = "Wayland has no protocol for listing windows. Capture a display instead.";
        let err = CliError::Core(CoreError::Unsupported {
            what: "window enumeration".into(),
            why: why.into(),
        });
        let text = err.to_human();
        assert!(text.contains(why));
        assert!(text.contains("window enumeration"));
        // Never presented as a crash.
        assert!(text.contains("not a fault in Scrozz"));
    }

    #[test]
    fn json_error_shape_is_stable() {
        let err = CliError::Core(CoreError::PermissionDenied {
            capability: "screen recording".into(),
            remedy: "System Settings".into(),
        });
        assert_eq!(
            err.to_json().to_compact_string(),
            r#"{"kind":"permission-denied","code":4,"message":"permission denied: screen recording (grant: System Settings)","cancelled":false,"actionable":true,"details":{"capability":"screen recording","remedy":"System Settings"}}"#
        );
    }

    #[test]
    fn json_error_always_has_the_same_outer_keys() {
        let mut cases: Vec<CliError> = every_core_error().into_iter().map(CliError::Core).collect();
        cases.push(CliError::usage("x"));
        cases.push(CliError::not_implemented("x", "y"));
        cases.push(CliError::ipc("x"));

        for err in cases {
            let Json::Obj(pairs) = err.to_json() else {
                panic!("error must serialise as an object")
            };
            let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
            assert_eq!(
                keys,
                [
                    "kind",
                    "code",
                    "message",
                    "cancelled",
                    "actionable",
                    "details"
                ],
                "outer error shape drifted for {err:?}"
            );
        }
    }

    #[test]
    fn json_kind_always_matches_the_exit_slug() {
        let mut cases: Vec<CliError> = every_core_error().into_iter().map(CliError::Core).collect();
        cases.push(CliError::usage("x"));
        cases.push(CliError::not_implemented("x", "y"));
        cases.push(CliError::ipc("x"));
        for err in cases {
            assert_eq!(err.kind(), err.exit().slug());
        }
    }

    #[test]
    fn unsupported_details_carry_what_and_why() {
        let err = CliError::Core(CoreError::Unsupported {
            what: "global hotkeys".into(),
            why: "wlroots has no GlobalShortcuts portal".into(),
        });
        assert_eq!(
            err.details().to_compact_string(),
            r#"{"what":"global hotkeys","why":"wlroots has no GlobalShortcuts portal"}"#
        );
    }

    #[test]
    fn simulated_errors_cover_every_exit_status_that_can_be_produced() {
        let names = [
            "usage",
            "cancelled",
            "permission-denied",
            "unsupported",
            "target-gone",
            "invalid-request",
            "codec",
            "storage",
            "io",
            "platform",
            "not-implemented",
            "ipc-failed",
        ];
        for name in names {
            let err = simulated_error_from(name);
            assert_eq!(
                err.kind(),
                name,
                "simulated {name:?} classified as {:?}",
                err.kind()
            );
        }
    }

    #[test]
    fn an_unknown_simulated_error_is_a_loud_usage_error() {
        let err = simulated_error_from("nonsense");
        assert_eq!(err.exit(), Exit::Usage);
        assert!(err.to_string().contains("nonsense"));
    }

    #[test]
    fn io_errors_convert_into_the_io_class() {
        let err: CliError = std::io::Error::other("boom").into();
        assert_eq!(err.exit(), Exit::Io);
    }

    #[test]
    fn the_permission_remedy_names_a_real_pane() {
        let remedy = permission_remedy_hint();
        assert!(!remedy.is_empty());
        // "check your settings" is exactly the useless text this avoids.
        assert!(remedy.len() > 20, "remedy is too vague: {remedy}");
    }
}
