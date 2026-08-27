//! Entering a COM/WinRT apartment, and leaving exactly what was entered.
//!
//! # Why this exists
//!
//! Every WinRT API — `Windows.Graphics.Capture`, `Windows.Media.Ocr` — requires
//! the calling thread to be in a COM apartment. A thread that is not gets
//! `CO_E_NOTINITIALIZED` from the very first call.
//!
//! That would be unremarkable if it failed loudly. It does not. The idiom that
//! grows around a `Result`-returning capability probe is
//! `GraphicsCaptureSession::IsSupported().unwrap_or(false)`, and on an
//! uninitialised thread that reads `CO_E_NOTINITIALIZED` as "this machine
//! cannot do WGC". Scrozz then falls back to GDI — losing cursor control,
//! per-window capture and alpha — on hardware that supports WGC perfectly well,
//! and reports nothing, because from its point of view nothing went wrong.
//!
//! `apps/scrozz` runs its capture worker on a plain `std::thread`, which enters
//! no apartment. So that is precisely the bug that was there.
//!
//! # Why it is not simply "call `RoInitialize` at start-up"
//!
//! Apartments are per *thread*, not per process. Initialising the main thread
//! does nothing for a worker.
//!
//! And the main thread is already spoken for: winit calls `OleInitialize` on
//! the event-loop thread to register drag-and-drop, which puts it in a
//! single-threaded apartment. Asking for an MTA there returns
//! `RPC_E_CHANGED_MODE`. That call did **not** initialise WinRT. Scrozz retries
//! with `RO_INIT_SINGLETHREADED`; only that successful call makes WinRT usable,
//! increments the apartment reference count, and earns a matching
//! `RoUninitialize`.
//!
//! Treating changed-mode as success reaches WinRT without initialising it.
//! Calling `RoUninitialize` for the failed attempt instead breaks somebody
//! else's apartment. The successful retry avoids both errors.
//!
//! # The rule, in one line
//!
//! Uninitialise if and only if this call initialised. [`ApartmentEntry`] is the
//! type that says which happened, and it is decided by pure code in
//! [`crate::win32`] that the host's tests actually run.

use scrozz_core::{Error, Result};
use windows::Win32::System::WinRT::{
    RO_INIT_MULTITHREADED, RO_INIT_SINGLETHREADED, RO_INIT_TYPE, RoInitialize, RoUninitialize,
};

use crate::win32::{ApartmentEntry, classify_apartment_entry};

/// Which apartment model to ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApartmentModel {
    /// Multi-threaded. The right answer for a worker thread.
    ///
    /// An MTA thread has no message pump and no marshalling requirements, which
    /// is exactly what a capture or OCR worker wants: it blocks on WinRT
    /// operations and never runs UI.
    Multi,
    /// Single-threaded. Only for a thread that pumps messages.
    ///
    /// Scrozz never asks for this — winit already establishes an STA on the one
    /// thread that qualifies — but naming it keeps [`Apartment::enter`] honest
    /// about what it is choosing rather than hard-coding a constant.
    Single,
}

impl ApartmentModel {
    const fn raw(self) -> RO_INIT_TYPE {
        match self {
            Self::Multi => RO_INIT_MULTITHREADED,
            Self::Single => RO_INIT_SINGLETHREADED,
        }
    }

    const fn other(self) -> Self {
        match self {
            Self::Multi => Self::Single,
            Self::Single => Self::Multi,
        }
    }
}

/// A thread's membership of a COM/WinRT apartment, released on drop.
///
/// Holds no pointer and no handle: the apartment is a property of the *thread*,
/// which is why this type is deliberately neither `Send` nor `Sync`. Moving it
/// to another thread and dropping it there would uninitialise an apartment on a
/// thread that never entered one, and leave the original thread's apartment
/// permanently referenced. The `PhantomData` below is what makes that a compile
/// error rather than a very confusing afternoon.
#[derive(Debug)]
pub struct Apartment {
    entry: ApartmentEntry,
    /// Pins the guard to its thread; see the type docs.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Apartment {
    /// Enters `model` on the calling thread.
    ///
    /// If the requested model conflicts with the thread's existing model, this
    /// retries with the other model. Success always means a `RoInitialize` call
    /// succeeded and this guard owns the matching reference.
    ///
    /// # Errors
    ///
    /// [`Error::Platform`] only when the thread ends up in no apartment at all,
    /// which in practice means the WinRT core is unavailable (Windows 7) or the
    /// process is out of memory.
    pub fn enter(model: ApartmentModel) -> Result<Self> {
        // SAFETY: `RoInitialize` takes only an enum by value and affects the
        // calling thread. It is safe to call repeatedly and from any thread.
        let first = initialise(model);
        let entry = classify_apartment_entry(first);
        match entry {
            ApartmentEntry::Failed(code) => Err(Error::Platform(format!(
                "entering a COM apartment: Windows refused (0x{code:08X}); \
                 screen capture and text recognition both need one"
            ))),
            ApartmentEntry::Entered => Ok(Self {
                entry,
                _not_send: std::marker::PhantomData,
            }),
            ApartmentEntry::RetryOtherModel => {
                let retry_model = model.other();
                let retry = initialise(retry_model);
                match classify_apartment_entry(retry) {
                    ApartmentEntry::Entered => Ok(Self {
                        entry: ApartmentEntry::Entered,
                        _not_send: std::marker::PhantomData,
                    }),
                    ApartmentEntry::RetryOtherModel | ApartmentEntry::Failed(_) => {
                        Err(Error::Platform(format!(
                            "entering a COM apartment: {model:?} returned \
                             RPC_E_CHANGED_MODE and retrying {retry_model:?} \
                             failed (0x{retry:08X}); WinRT is not initialised \
                             on this thread"
                        )))
                    }
                }
            }
        }
    }

    /// Enters a multi-threaded apartment. The worker-thread default.
    ///
    /// # Errors
    ///
    /// As [`Self::enter`].
    pub fn enter_multithreaded() -> Result<Self> {
        Self::enter(ApartmentModel::Multi)
    }

    /// What actually happened.
    #[must_use]
    pub const fn entry(&self) -> ApartmentEntry {
        self.entry
    }

    /// Whether this guard owns the apartment it is holding.
    ///
    /// Every returned guard owns one successful `RoInitialize`, including one
    /// obtained by retrying with winit's existing STA model.
    #[must_use]
    pub const fn owns(&self) -> bool {
        self.entry.owes_uninitialise()
    }
}

fn initialise(model: ApartmentModel) -> i32 {
    // SAFETY: `RoInitialize` takes only a documented enum by value and affects
    // the calling thread. It is safe to retry with the matching model.
    match unsafe { RoInitialize(model.raw()) } {
        Ok(()) => 0,
        Err(err) => err.code().0,
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if !self.entry.owes_uninitialise() {
            return;
        }
        // SAFETY: balanced against exactly one successful `RoInitialize` on
        // this thread, and the guard cannot have moved to another thread
        // because it is neither `Send` nor `Sync`.
        unsafe { RoUninitialize() };
    }
}

/// Runs `body` inside a multi-threaded apartment on the calling thread.
///
/// The shape worker threads should use, because it makes the lifetime
/// unambiguous by construction: the apartment covers exactly the closure, and
/// it is released when the closure returns however it returns.
///
/// # Errors
///
/// [`Error::Platform`] if no apartment could be entered; otherwise whatever
/// `body` returns.
pub fn with_multithreaded_apartment<T, F>(body: F) -> Result<T>
where
    F: FnOnce() -> Result<T>,
{
    let apartment = Apartment::enter_multithreaded()?;
    let result = body();
    drop(apartment);
    result
}
