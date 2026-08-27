//! macOS recording error classification.

use objc2_foundation::NSError;
use objc2_screen_capture_kit::SCStreamErrorCode;
use scrozz_core::Error;

pub(crate) const SCREEN_REMEDY: &str = "System Settings -> Privacy & Security -> \
    Screen & System Audio Recording (called \"Screen Recording\" before macOS 15): \
    switch Scrozz on, then quit and reopen Scrozz so macOS re-checks the grant";

pub(crate) const MICROPHONE_REMEDY: &str =
    "System Settings -> Privacy & Security -> Microphone: switch Scrozz on";

pub(crate) fn screen_permission_denied() -> Error {
    Error::PermissionDenied {
        capability: "screen recording".to_owned(),
        remedy: SCREEN_REMEDY.to_owned(),
    }
}

pub(crate) fn microphone_permission_denied() -> Error {
    Error::PermissionDenied {
        capability: "microphone".to_owned(),
        remedy: MICROPHONE_REMEDY.to_owned(),
    }
}

pub(crate) fn from_sck(error: &NSError, context: &str) -> Error {
    let detail = describe(error, context);
    match SCStreamErrorCode(error.code()) {
        SCStreamErrorCode::UserDeclined | SCStreamErrorCode::MissingEntitlements => {
            screen_permission_denied()
        }
        SCStreamErrorCode::NoWindowList
        | SCStreamErrorCode::NoDisplayList
        | SCStreamErrorCode::NoCaptureSource
        | SCStreamErrorCode::FailedNoMatchingApplicationContext => Error::TargetGone(detail),
        SCStreamErrorCode::InvalidParameter => Error::InvalidRequest(detail),
        _ => Error::Platform(detail),
    }
}

pub(crate) fn describe(error: &NSError, context: &str) -> String {
    let message = error.localizedDescription().to_string();
    format!("{context}: {message} (code {})", error.code())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_screen_permission_remedy_names_both_versions_of_the_settings_pane() {
        assert!(SCREEN_REMEDY.contains("Screen & System Audio Recording"));
        assert!(SCREEN_REMEDY.contains("Screen Recording"));
        assert!(SCREEN_REMEDY.contains("reopen"));
    }
}
