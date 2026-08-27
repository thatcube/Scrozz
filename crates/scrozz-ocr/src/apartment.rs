//! Telling "you have no language pack" apart from "this thread has no COM".
//!
//! # Why this exists
//!
//! `Windows.Media.Ocr` is WinRT, so every call on a thread that never entered a
//! COM apartment fails with `CO_E_NOTINITIALIZED`. The natural mapping for a
//! failing `OcrEngine::TryCreateFromUserProfileLanguages()` is
//! [`Error::Unsupported`] with the remedy "install a language pack" — which is
//! usually right, and on an uninitialised thread is actively harmful: it sends
//! the user to Settings to install something they already have, to fix a
//! problem that is not theirs.
//!
//! D15 asks that an error tell its reader what to do. That obliges Scrozz to
//! know which reader it is talking to. `CO_E_NOTINITIALIZED` is a message for a
//! developer; everything else here is a message for the user.
//!
//! Kept free of the `windows` crate so the mapping is checked by tests on
//! whatever machine happens to be building, which — for this project — is not
//! Windows.

use scrozz_core::Error;

/// `CO_E_NOTINITIALIZED` — the calling thread is in no COM apartment.
pub const CO_E_NOTINITIALIZED: i32 = 0x8004_01F0_u32 as i32;

/// `RPC_E_CHANGED_MODE` — the thread is in an apartment, of the other model.
///
/// Not a failure: WinRT works in either model. It appears here only so that a
/// reader of this module does not mistake it for one.
pub const RPC_E_CHANGED_MODE: i32 = 0x8001_0106_u32 as i32;

/// Whether an HRESULT means "this thread never entered a COM apartment".
#[must_use]
pub const fn is_uninitialised_apartment(hresult: i32) -> bool {
    hresult == CO_E_NOTINITIALIZED
}

/// The error for a failed OCR engine construction.
///
/// `detail` is the platform's own message, kept so a bug report carries it.
/// `what` names the capability in the user's terms.
#[must_use]
pub fn engine_failure(hresult: i32, what: &str, detail: &str) -> Error {
    if is_uninitialised_apartment(hresult) {
        // Addressed to a developer, because there is nothing the user can do:
        // the thread simply was not set up before it asked.
        return Error::Platform(format!(
            "{what} was requested from a thread with no COM apartment \
             (CO_E_NOTINITIALIZED). This is a Scrozz bug, not a missing language \
             pack: the calling thread must enter an apartment before using \
             Windows.Media.Ocr. Original message: {detail}"
        ));
    }
    // Addressed to the user, because this one really is theirs to fix.
    Error::Unsupported {
        what: what.to_owned(),
        why: format!(
            "Windows has no OCR language pack for your display languages. \
             Add one in Settings > Time & language > Language & region > \
             Add a language, choosing a language whose optional features \
             include Optical character recognition ({detail})"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CO_E_NOTINITIALIZED, RPC_E_CHANGED_MODE, engine_failure, is_uninitialised_apartment,
    };
    use scrozz_core::Error;

    #[test]
    fn only_co_e_notinitialized_means_no_apartment() {
        assert!(is_uninitialised_apartment(CO_E_NOTINITIALIZED));
        // The thread *is* in an apartment, just not the one that was asked for.
        assert!(!is_uninitialised_apartment(RPC_E_CHANGED_MODE));
        assert!(!is_uninitialised_apartment(0));
        assert!(!is_uninitialised_apartment(0x8007_000E_u32 as i32));
    }

    #[test]
    fn a_missing_apartment_does_not_blame_the_users_language_settings() {
        // The bug this guards: telling somebody to install the English pack
        // they already have, because a worker thread was never initialised.
        let err = engine_failure(CO_E_NOTINITIALIZED, "text recognition", "boom");
        match &err {
            Error::Platform(message) => {
                assert!(message.contains("apartment"), "{message}");
                assert!(message.contains("Scrozz bug"), "{message}");
                assert!(
                    !message.contains("Settings"),
                    "must not send the user shopping for a language pack: {message}"
                );
                assert!(message.contains("boom"), "keeps the original: {message}");
            }
            other => panic!("a developer-facing fault must not be Unsupported: {other:?}"),
        }
    }

    #[test]
    fn a_genuinely_missing_pack_still_tells_the_user_how_to_install_one() {
        let err = engine_failure(0x8007_000E_u32 as i32, "text recognition", "boom");
        match &err {
            Error::Unsupported { what, why } => {
                assert_eq!(what, "text recognition");
                assert!(why.contains("Settings"), "{why}");
                assert!(why.contains("Optical character recognition"), "{why}");
                assert!(why.contains("boom"), "{why}");
            }
            other => panic!("the common case must stay actionable for the user: {other:?}"),
        }
    }

    #[test]
    fn the_two_readerships_never_receive_each_others_advice() {
        let developer = engine_failure(CO_E_NOTINITIALIZED, "text recognition", "d");
        let user = engine_failure(-1, "text recognition", "d");
        assert!(matches!(developer, Error::Platform(_)));
        assert!(matches!(user, Error::Unsupported { .. }));
    }
}
