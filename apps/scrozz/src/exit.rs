//! Exit codes.
//!
//! # Why these are granular
//!
//! Per decision D11 the CLI is how hotkeys work at all on wlroots, how the app
//! is scripted, and how agents verify it. All three consumers are programs, and
//! a program's only cheap channel is the exit status.
//!
//! The requirement that shapes the whole table is this: **a script must be able
//! to tell "the user pressed Escape" from "the app is broken".** Decision D15
//! and [`scrozz_core::Error::Cancelled`] both say cancellation is an outcome and
//! not a fault, so collapsing it into a generic `1` would make every wrapper
//! script report a spurious failure every time someone changed their mind.
//!
//! The same argument applies to [`scrozz_core::Error::PermissionDenied`], which
//! carries a remedy the user can act on, and to
//! [`scrozz_core::Error::Unsupported`], which per D8 is a documented platform
//! gap rather than a defect. A caller that retries on transient failure must not
//! retry either of those.
//!
//! # Stability
//!
//! These values are a public contract. Existing codes never change meaning; new
//! error classes take new numbers. Codes stay below 64 so they cannot collide
//! with the shell's own conventions (`126` "not executable", `127` "not found",
//! `128 + signal`).

use std::process::ExitCode;

/// The exit status of a Scrozz invocation.
///
/// Each variant corresponds to exactly one error class, so the mapping in
/// [`crate::fault::CliError::exit`] is total and can be tested exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Exit {
    /// The command completed.
    Success = 0,

    /// A failure with no more specific classification.
    ///
    /// Also the destination for any future [`scrozz_core::Error`] variant: that
    /// enum is `#[non_exhaustive]`, so a build against a newer core must still
    /// produce a defined status rather than fail to compile.
    Failure = 1,

    /// The arguments were not usable.
    ///
    /// Fixed at `2` because `clap` exits with `2` on a parse failure and cannot
    /// be told otherwise without intercepting every error path. Matching it
    /// means "bad invocation" is one number whether the objection came from the
    /// parser or from a later semantic check.
    Usage = 2,

    /// The user cancelled — Escape during region selection, or a dismissed
    /// portal picker.
    ///
    /// **Not a fault.** Scripts should treat this as "nothing to do".
    Cancelled = 3,

    /// The OS withheld a capability the user must grant.
    ///
    /// Distinct because it is the only status with a remedy: stderr carries the
    /// exact settings pane to open.
    PermissionDenied = 4,

    /// The capability does not exist on this platform or compositor.
    ///
    /// Per D8 this is a documented gap — window enumeration on Wayland, global
    /// shortcuts on wlroots — and never a crash. Retrying will never help.
    Unsupported = 5,

    /// The requested window or display vanished between enumeration and use.
    ///
    /// The one class where an immediate retry is usually right.
    TargetGone = 6,

    /// The request parsed but was incoherent, e.g. a zero-area region.
    ///
    /// Separate from [`Self::Usage`]: the invocation was well-formed, its
    /// meaning was not.
    InvalidRequest = 7,

    /// Encoding or decoding failed.
    Codec = 8,

    /// Persistent storage failed.
    Storage = 9,

    /// An I/O failure — an unwritable output path, a broken pipe.
    Io = 10,

    /// A platform API failed with no better classification.
    Platform = 11,

    /// The capability is real and specified but not yet wired up.
    ///
    /// Temporary by construction, and deliberately not [`Self::Failure`]: while
    /// the workspace is a tree of contracts, an agent needs to distinguish "this
    /// is not built yet" from "this is built and broken".
    NotImplemented = 12,

    /// A single-instance request could not reach the running instance.
    ///
    /// Distinct from [`Self::Io`] because the remedy is different: the socket
    /// was there and the handshake failed, so the running instance is wedged or
    /// is a different version.
    IpcFailed = 13,
}

impl Exit {
    /// The numeric status.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// A stable machine-readable slug, mirrored by the JSON error `kind`.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Success => "ok",
            Self::Failure => "failure",
            Self::Usage => "usage",
            Self::Cancelled => "cancelled",
            Self::PermissionDenied => "permission-denied",
            Self::Unsupported => "unsupported",
            Self::TargetGone => "target-gone",
            Self::InvalidRequest => "invalid-request",
            Self::Codec => "codec",
            Self::Storage => "storage",
            Self::Io => "io",
            Self::Platform => "platform",
            Self::NotImplemented => "not-implemented",
            Self::IpcFailed => "ipc-failed",
        }
    }

    /// Whether this status describes an ordinary outcome rather than a fault.
    ///
    /// Wrapper scripts branch on this: a cancelled capture should not print an
    /// error, page anyone, or fail a build.
    #[must_use]
    pub const fn is_fault(self) -> bool {
        !matches!(self, Self::Success | Self::Cancelled)
    }

    /// Every status, for exhaustive tests.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Success,
            Self::Failure,
            Self::Usage,
            Self::Cancelled,
            Self::PermissionDenied,
            Self::Unsupported,
            Self::TargetGone,
            Self::InvalidRequest,
            Self::Codec,
            Self::Storage,
            Self::Io,
            Self::Platform,
            Self::NotImplemented,
            Self::IpcFailed,
        ]
    }
}

impl From<Exit> for ExitCode {
    fn from(value: Exit) -> Self {
        Self::from(value.code())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn success_is_zero_and_usage_matches_clap() {
        assert_eq!(Exit::Success.code(), 0);
        // clap hardcodes 2 for a parse failure; if this ever drifts the CLI
        // would report two different numbers for one meaning.
        assert_eq!(Exit::Usage.code(), 2);
    }

    #[test]
    fn cancellation_is_distinguishable_from_failure() {
        // The single most load-bearing property of the whole table.
        assert_ne!(Exit::Cancelled.code(), Exit::Failure.code());
        assert_ne!(Exit::Cancelled.code(), Exit::Success.code());
    }

    #[test]
    fn every_code_is_unique() {
        let codes: BTreeSet<u8> = Exit::all().iter().map(|e| e.code()).collect();
        assert_eq!(
            codes.len(),
            Exit::all().len(),
            "two statuses share a number, so a script cannot tell them apart"
        );
    }

    #[test]
    fn every_slug_is_unique() {
        let slugs: BTreeSet<&str> = Exit::all().iter().map(|e| e.slug()).collect();
        assert_eq!(slugs.len(), Exit::all().len());
    }

    #[test]
    fn codes_avoid_shell_reserved_range() {
        for exit in Exit::all() {
            assert!(
                exit.code() < 64,
                "{:?} = {} collides with shell conventions (126/127/128+n)",
                exit,
                exit.code()
            );
        }
    }

    #[test]
    fn only_success_and_cancellation_are_not_faults() {
        let non_faults: Vec<_> = Exit::all().iter().filter(|e| !e.is_fault()).collect();
        assert_eq!(non_faults, vec![&Exit::Success, &Exit::Cancelled]);
    }

    #[test]
    fn documented_numbers_are_pinned() {
        // These are a public contract. Changing one silently breaks every
        // wrapper script in the wild, so the numbers are asserted literally.
        assert_eq!(Exit::Success.code(), 0);
        assert_eq!(Exit::Failure.code(), 1);
        assert_eq!(Exit::Usage.code(), 2);
        assert_eq!(Exit::Cancelled.code(), 3);
        assert_eq!(Exit::PermissionDenied.code(), 4);
        assert_eq!(Exit::Unsupported.code(), 5);
        assert_eq!(Exit::TargetGone.code(), 6);
        assert_eq!(Exit::InvalidRequest.code(), 7);
        assert_eq!(Exit::Codec.code(), 8);
        assert_eq!(Exit::Storage.code(), 9);
        assert_eq!(Exit::Io.code(), 10);
        assert_eq!(Exit::Platform.code(), 11);
        assert_eq!(Exit::NotImplemented.code(), 12);
        assert_eq!(Exit::IpcFailed.code(), 13);
    }
}
