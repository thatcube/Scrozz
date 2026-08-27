//! Balanced COM/WinRT apartment entry for the calling thread.

use scrozz_core::{Error, Result};
use windows::Win32::System::WinRT::{
    RO_INIT_MULTITHREADED, RO_INIT_SINGLETHREADED, RO_INIT_TYPE, RoInitialize, RoUninitialize,
};

use crate::win32::{ApartmentEntry, classify_apartment_entry};

/// Which apartment model to request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApartmentModel {
    /// Multi-threaded worker apartment.
    Multi,
    /// Single-threaded message-loop apartment.
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

/// Calling-thread apartment membership, balanced on drop.
#[derive(Debug)]
pub struct Apartment {
    entry: ApartmentEntry,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Apartment {
    /// Enters an apartment, retrying the already-established model when needed.
    pub fn enter(model: ApartmentModel) -> Result<Self> {
        let first = initialise(model);
        match classify_apartment_entry(first) {
            ApartmentEntry::Entered => Ok(Self::entered()),
            ApartmentEntry::RetryOtherModel => {
                let retry_model = model.other();
                let retry = initialise(retry_model);
                match classify_apartment_entry(retry) {
                    ApartmentEntry::Entered => Ok(Self::entered()),
                    ApartmentEntry::RetryOtherModel | ApartmentEntry::Failed(_) => {
                        Err(Error::Platform(format!(
                            "entering a COM apartment: {model:?} returned RPC_E_CHANGED_MODE and \
                             retrying {retry_model:?} failed (0x{:08X})",
                            retry.cast_unsigned()
                        )))
                    }
                }
            }
            ApartmentEntry::Failed(code) => Err(Error::Platform(format!(
                "entering a COM apartment failed (0x{:08X})",
                code.cast_unsigned()
            ))),
        }
    }

    /// Enters the worker-thread default apartment.
    pub fn enter_multithreaded() -> Result<Self> {
        Self::enter(ApartmentModel::Multi)
    }

    /// Whether this guard owns a matching `RoUninitialize`.
    #[must_use]
    pub const fn owns(&self) -> bool {
        self.entry.owes_uninitialise()
    }

    const fn entered() -> Self {
        Self {
            entry: ApartmentEntry::Entered,
            _not_send: std::marker::PhantomData,
        }
    }
}

fn initialise(model: ApartmentModel) -> i32 {
    match unsafe { RoInitialize(model.raw()) } {
        Ok(()) => 0,
        Err(error) => error.code().0,
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.entry.owes_uninitialise() {
            unsafe { RoUninitialize() };
        }
    }
}
