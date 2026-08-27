//! Errors crossing Scrozz crate boundaries.

use thiserror::Error;

/// The result type used throughout Scrozz.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A failure in any Scrozz subsystem.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The OS withheld a capability the user must grant.
    ///
    /// Distinct from every other error because it is the only one with a
    /// *remedy the user can act on*, and per decision D15 it is expected on the
    /// first use of a feature rather than exceptional. Callers surface this as
    /// guidance, never as a crash.
    #[error("permission denied: {capability} (grant: {remedy})")]
    PermissionDenied {
        /// What was refused, e.g. "screen recording".
        capability: String,
        /// Where the user grants it, in their platform's own words.
        remedy: String,
    },

    /// The feature cannot work on this platform or compositor.
    ///
    /// Wayland forces this to be a first-class outcome: window enumeration has
    /// no protocol at all, and global hotkeys are unavailable on wlroots
    /// compositors. Per D8 these gaps are documented and degraded gracefully,
    /// never presented as a crash or a bug.
    #[error("unsupported on this platform: {what} ({why})")]
    Unsupported {
        /// The capability requested.
        what: String,
        /// Why it is unavailable here, and any alternative.
        why: String,
    },

    /// The requested display, window or capture no longer exists.
    #[error("target no longer exists: {0}")]
    TargetGone(String),

    /// A capture was requested with incoherent parameters.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Encoding or decoding failed.
    #[error("codec error: {0}")]
    Codec(String),

    /// Persistent storage failed.
    #[error("storage error: {0}")]
    Storage(String),

    /// The user cancelled, e.g. pressing Escape during region selection.
    ///
    /// An outcome, not a fault. It exists as an error variant only so it can
    /// propagate through `?`; callers must not report it to the user.
    #[error("cancelled by user")]
    Cancelled,

    /// An underlying I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A platform API failed in a way with no better classification.
    #[error("platform error: {0}")]
    Platform(String),
}

impl Clone for Error {
    fn clone(&self) -> Self {
        match self {
            Self::PermissionDenied { capability, remedy } => Self::PermissionDenied {
                capability: capability.clone(),
                remedy: remedy.clone(),
            },
            Self::Unsupported { what, why } => Self::Unsupported {
                what: what.clone(),
                why: why.clone(),
            },
            Self::TargetGone(message) => Self::TargetGone(message.clone()),
            Self::InvalidRequest(message) => Self::InvalidRequest(message.clone()),
            Self::Codec(message) => Self::Codec(message.clone()),
            Self::Storage(message) => Self::Storage(message.clone()),
            Self::Cancelled => Self::Cancelled,
            Self::Io(error) => Self::Io(error.raw_os_error().map_or_else(
                || std::io::Error::new(error.kind(), error.to_string()),
                std::io::Error::from_raw_os_error,
            )),
            Self::Platform(message) => Self::Platform(message.clone()),
        }
    }
}

impl Error {
    /// Whether this represents ordinary user cancellation rather than a fault.
    #[must_use]
    pub const fn is_cancellation(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// Whether the user could plausibly fix this by granting a permission.
    #[must_use]
    pub const fn is_actionable_by_user(&self) -> bool {
        matches!(self, Self::PermissionDenied { .. })
    }
}
