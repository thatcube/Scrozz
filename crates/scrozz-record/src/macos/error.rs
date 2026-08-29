//! macOS recording error classification.

use objc2_foundation::{NSError, NSUnderlyingErrorKey};
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
    format!("{context}: {}", describe_error(error, true))
}

fn describe_error(error: &NSError, include_underlying: bool) -> String {
    let message = error.localizedDescription().to_string();
    let mut detail = format!("{message} ({} code {})", error.domain(), error.code());
    if let Some(hint) = os_status_hint(error.code()) {
        detail.push_str(hint);
    }
    if let Some(reason) = error.localizedFailureReason() {
        let reason = reason.to_string();
        if !reason.is_empty() && reason != message {
            detail.push_str(&format!(": {reason}"));
        }
    }
    if include_underlying {
        // SAFETY: immutable weak-linked Foundation user-info key.
        let key = unsafe { NSUnderlyingErrorKey };
        if let Some(underlying) = error
            .userInfo()
            .objectForKey(key)
            .and_then(|value| value.downcast::<NSError>().ok())
        {
            detail.push_str(&format!(
                "; underlying {}",
                describe_error(&underlying, false)
            ));
        }
    }
    detail
}

fn os_status_hint(code: isize) -> Option<&'static str> {
    (code == -16_341).then_some(
        " [private OSStatus 0xffffc02b: no public SDK symbol; the hardware video encoder rejected the submitted frame]",
    )
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

    #[test]
    fn private_hardware_encoder_status_is_named_without_inventing_a_symbol() {
        let hint = os_status_hint(-16_341).unwrap();
        assert!(hint.contains("0xffffc02b"));
        assert!(hint.contains("no public SDK symbol"));
        assert!(hint.contains("hardware video encoder"));
    }
}
