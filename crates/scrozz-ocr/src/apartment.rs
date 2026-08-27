//! Telling "you have no language pack" apart from "this thread has no COM".
//!
//! `Windows.Media.Ocr` is WinRT, so every call on a thread that never entered a
//! COM apartment fails with `CO_E_NOTINITIALIZED`. The guard in this module
//! makes the OCR crate safe to call without relying on every embedder to
//! remember that thread prerequisite.

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

/// Maps OCR construction failures to developer- or user-facing errors.
#[must_use]
pub fn engine_failure(hresult: i32, what: &str, detail: &str) -> Error {
    if is_uninitialised_apartment(hresult) {
        return Error::Platform(format!(
            "{what} was requested from a thread with no COM apartment \
             (CO_E_NOTINITIALIZED). This is a Scrozz bug, not a missing language \
             pack: the calling thread must enter an apartment before using \
             Windows.Media.Ocr. Original message: {detail}"
        ));
    }
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
/// The guard is deliberately neither `Send` nor `Sync`, so the balancing
/// `RoUninitialize` cannot run on another thread.
#[cfg(target_os = "windows")]
#[derive(Debug)]
pub struct Apartment {
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(target_os = "windows")]
impl Apartment {
    /// Enters a WinRT apartment on the calling thread.
    ///
    /// If another component already selected an STA, the matching model is
    /// retried rather than changing it.
    pub fn enter_multithreaded() -> Result<Self> {
        // SAFETY: `RoInitialize` affects only this thread and receives a
        // documented initialization mode.
        match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            Ok(()) => Ok(Self {
                _not_send: std::marker::PhantomData,
            }),
            Err(error) if needs_apartment_model_retry(error.code().0) => {
                // SAFETY: the MTA attempt established that the thread already
                // uses the other model. A successful retry is balanced by Drop.
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
            Err(error) => Err(Error::Platform(format!(
                "entering a COM apartment for Windows OCR failed \
                 (0x{:08X}): {}; text recognition cannot run on this thread",
                error.code().0,
                error
            ))),
        }
    }

    /// Whether this guard owns a matching `RoUninitialize`.
    #[must_use]
    pub const fn owns(&self) -> bool {
        true
    }
}

#[cfg(target_os = "windows")]
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: constructors return a guard only after successful
        // `RoInitialize` on this same thread.
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
    fn a_missing_apartment_does_not_blame_language_settings() {
        let error = engine_failure(CO_E_NOTINITIALIZED, "text recognition", "boom");
        match &error {
            Error::Platform(message) => {
                assert!(message.contains("apartment"), "{message}");
                assert!(message.contains("Scrozz bug"), "{message}");
                assert!(!message.contains("Settings"), "{message}");
                assert!(message.contains("boom"), "{message}");
            }
            other => panic!("a developer-facing fault must be Platform: {other:?}"),
        }
    }

    #[test]
    fn a_missing_pack_still_explains_the_remedy() {
        let error = engine_failure(0x8007_000E_u32 as i32, "text recognition", "boom");
        match &error {
            Error::Unsupported { what, why } => {
                assert_eq!(what, "text recognition");
                assert!(why.contains("Settings"), "{why}");
                assert!(why.contains("Optical character recognition"), "{why}");
                assert!(why.contains("boom"), "{why}");
            }
            other => panic!("the user-facing fault must be Unsupported: {other:?}"),
        }
    }
}
