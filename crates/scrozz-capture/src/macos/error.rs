//! Turning macOS failure modes into [`scrozz_core::Error`].
//!
//! Two of these mappings are load-bearing rather than cosmetic.
//!
//! Screen Recording permission is the first: per decision D15 it is requested
//! at first use, so its absence is the *expected* state on a fresh install, not
//! a fault. It must arrive as [`scrozz_core::Error::PermissionDenied`] carrying
//! a remedy the user can actually follow, and it must never panic.
//!
//! A vanished target is the second: `SCShareableContent` is a snapshot, and a
//! window can close between the moment it is listed and the moment it is
//! captured. That is [`scrozz_core::Error::TargetGone`], not a crash.

use objc2::rc::Retained;
use objc2_foundation::NSError;
use objc2_screen_capture_kit::SCStreamErrorCode;
use scrozz_core::Error;

/// What the user must grant, in the OS's own vocabulary.
pub(crate) const CAPABILITY: &str = "screen recording";

/// Where to grant it.
///
/// Named as precisely as macOS itself names it. The pane was called "Screen
/// Recording" through macOS 14 and "Screen & System Audio Recording" from macOS
/// 15, so both appear: a remedy the user cannot find on their own machine is no
/// remedy at all. The relaunch note matters too — macOS does not retroactively
/// grant capture access to an already-running process.
pub(crate) const REMEDY: &str = "System Settings → Privacy & Security → \
     Screen & System Audio Recording (called “Screen Recording” before macOS 15): \
     switch Scrozz on, then quit and reopen Scrozz so macOS re-checks the grant";

/// The canonical permission error.
pub(crate) fn permission_denied() -> Error {
    Error::PermissionDenied {
        capability: CAPABILITY.to_owned(),
        remedy: REMEDY.to_owned(),
    }
}

/// This backend needs a macOS newer than the one it is running on.
pub(crate) fn unsupported(what: &str, why: &str) -> Error {
    Error::Unsupported {
        what: what.to_owned(),
        why: why.to_owned(),
    }
}

/// Classifies an `NSError` handed back by a ScreenCaptureKit completion handler.
///
/// The domain is not checked. In practice ScreenCaptureKit reports through
/// `SCStreamErrorDomain`, but it also forwards `NSCocoaErrorDomain` failures
/// from further down, and misclassifying one of those as "unknown platform
/// error" loses less than wrongly asserting a domain would.
pub(crate) fn from_ns_error(error: &NSError, context: &str) -> Error {
    let code = SCStreamErrorCode(error.code());
    let detail = describe(error, context);

    match code {
        // The user said no, or the app was never allowed to ask. Both are
        // fixed in the same place.
        SCStreamErrorCode::UserDeclined | SCStreamErrorCode::MissingEntitlements => {
            permission_denied()
        }

        // ScreenCaptureKit could not resolve the thing it was pointed at. A
        // window that closed while the picker was open lands here.
        SCStreamErrorCode::NoWindowList
        | SCStreamErrorCode::NoDisplayList
        | SCStreamErrorCode::NoCaptureSource
        | SCStreamErrorCode::FailedNoMatchingApplicationContext => Error::TargetGone(detail),

        SCStreamErrorCode::InvalidParameter => Error::InvalidRequest(detail),

        _ => Error::Platform(detail),
    }
}

/// Same, for the `Option<Retained<NSError>>` shape the bridge produces.
pub(crate) fn from_optional_ns_error(error: Option<Retained<NSError>>, context: &str) -> Error {
    error.map_or_else(
        || {
            Error::Platform(format!(
                "{context}: ScreenCaptureKit returned neither a result nor an error"
            ))
        },
        |error| from_ns_error(&error, context),
    )
}

fn describe(error: &NSError, context: &str) -> String {
    let code = error.code();
    let message = error.localizedDescription().to_string();
    format!("{context}: {message} (code {code})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_remedy_names_a_pane_the_user_can_find() {
        assert!(REMEDY.contains("System Settings"));
        assert!(REMEDY.contains("Privacy & Security"));
        assert!(REMEDY.contains("Screen"));
    }

    #[test]
    fn permission_denied_is_user_actionable() {
        assert!(permission_denied().is_actionable_by_user());
    }

    #[test]
    fn unsupported_is_not_a_cancellation() {
        let error = unsupported("what", "why");
        assert!(!error.is_cancellation());
        assert!(!error.is_actionable_by_user());
    }
}
