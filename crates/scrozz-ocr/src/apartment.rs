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
//! The classification below stays free of the `windows` crate so it is checked
//! by tests on whatever machine happens to be building. The small native guard
//! at the bottom is Windows-only and makes the OCR library safe to call without
//! relying on every embedder to remember an undocumented thread prerequisite.

use scrozz_core::Error;

#[cfg(target_os = "windows")]
use scrozz_core::Result;
#[cfg(target_os = "windows")]
use windows::Win32::System::WinRT::{
    RO_INIT_MULTITHREADED, RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize,
};

/// `CO_E_NOTINITIALIZED` — the calling thread is in no COM apartment.
pub const CO_E_NOTINITIALIZED: i32 = 0x8004_01F0_u32 as i32;

/// `RPC_E_CHANGED_MODE` — retry `RoInitialize` with the existing model.
///
/// The failed call neither initialises WinRT nor takes a reference.
pub const RPC_E_CHANGED_MODE: i32 = 0x8001_0106_u32 as i32;

/// Whether an HRESULT means "this thread never entered a COM apartment".
#[must_use]
pub const fn is_uninitialised_apartment(hresult: i32) -> bool {
    hresult == CO_E_NOTINITIALIZED
}

/// Whether `RoInitialize` must be retried with the other apartment model.
#[must_use]
pub const fn needs_apartment_model_retry(hresult: i32) -> bool {
    hresult == RPC_E_CHANGED_MODE
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

/// Membership of the calling thread in a COM apartment.
///
/// Windows OCR is WinRT, and apartments are per thread. The guard is kept for
/// the complete recognition operation and is deliberately neither `Send` nor
/// `Sync`, so the balancing `RoUninitialize` cannot run on a different thread.
#[cfg(target_os = "windows")]
#[derive(Debug)]
pub struct Apartment {
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(target_os = "windows")]
impl Apartment {
    /// Enters an MTA on the calling thread.
    ///
    /// `RPC_E_CHANGED_MODE` means a caller such as winit already selected an
    /// STA. Retry with that model; only a successful retry makes WinRT usable
    /// and earns the `RoUninitialize` performed by this guard.
    pub fn enter_multithreaded() -> Result<Self> {
        // SAFETY: `RoInitialize` affects only the calling thread and accepts this
        // documented enum value.
        match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            Ok(()) => Ok(Self {
                _not_send: std::marker::PhantomData,
            }),
            Err(err) if needs_apartment_model_retry(err.code().0) => {
                // SAFETY: the MTA attempt established that this thread already
                // has the other model. Retrying the documented STA value is the
                // only way to initialise WinRT without changing that model.
                unsafe { RoInitialize(RO_INIT_SINGLETHREADED) }
                    .map(|()| Self {
                        _not_send: std::marker::PhantomData,
                    })
                    .map_err(|retry| {
                        Error::Platform(format!(
                            "entering the existing STA for Windows OCR failed \
                             after RPC_E_CHANGED_MODE (0x{:08X}): {}; text \
                             recognition cannot run on this thread",
                            retry.code().0,
                            retry
                        ))
                    })
            }
            Err(err) => Err(Error::Platform(format!(
                "entering a COM apartment for Windows OCR failed \
                 (0x{:08X}): {}; text recognition cannot run on this thread",
                err.code().0,
                err
            ))),
        }
    }

    /// Whether this guard owes the thread a matching `RoUninitialize`.
    #[must_use]
    pub const fn owns(&self) -> bool {
        true
    }
}

#[cfg(target_os = "windows")]
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: constructors return a guard only after a successful
        // `RoInitialize` on this thread; the guard cannot be sent elsewhere.
        unsafe { RoUninitialize() };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CO_E_NOTINITIALIZED, RPC_E_CHANGED_MODE, engine_failure, is_uninitialised_apartment,
        needs_apartment_model_retry,
    };
    use scrozz_core::Error;

    #[test]
    fn only_co_e_notinitialized_means_no_apartment() {
        assert!(is_uninitialised_apartment(CO_E_NOTINITIALIZED));
        // COM already selected a model, but this failed call did not initialise
        // WinRT and needs a matching-model retry.
        assert!(!is_uninitialised_apartment(RPC_E_CHANGED_MODE));
        assert!(!is_uninitialised_apartment(0));
        assert!(!is_uninitialised_apartment(0x8007_000E_u32 as i32));
    }

    #[test]
    fn only_rpc_e_changed_mode_requests_the_other_model() {
        assert!(needs_apartment_model_retry(RPC_E_CHANGED_MODE));
        assert!(!needs_apartment_model_retry(CO_E_NOTINITIALIZED));
        assert!(!needs_apartment_model_retry(0));
        assert!(!needs_apartment_model_retry(1));
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
