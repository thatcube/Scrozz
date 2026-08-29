//! Paired COM/WinRT and Media Foundation process state.

use scrozz_core::{Error, Result};
use windows::Win32::{
    Foundation::RPC_E_CHANGED_MODE,
    Media::MediaFoundation::{MF_VERSION, MFSTARTUP_NOSOCKET, MFShutdown, MFStartup},
    System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
};

/// A worker-thread WinRT apartment.
pub struct Apartment {
    uninitialize: bool,
}

impl Apartment {
    /// Enters the multithreaded apartment.
    pub fn enter() -> Result<Self> {
        match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
            Ok(()) => Ok(Self { uninitialize: true }),
            Err(error) if error.code() == RPC_E_CHANGED_MODE => {
                // The embedding process already chose an apartment. It owns the
                // matching uninitialization, so this guard must not call it.
                Ok(Self {
                    uninitialize: false,
                })
            }
            Err(error) => Err(Error::Platform(format!("RoInitialize failed: {error}"))),
        }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.uninitialize {
            unsafe { RoUninitialize() };
        }
    }
}

/// One balanced Media Foundation startup on the recording worker.
pub struct MediaFoundation;

impl MediaFoundation {
    /// Starts Media Foundation without network sockets.
    pub fn start() -> Result<Self> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }
            .map_err(|error| Error::Platform(format!("MFStartup failed: {error}")))?;
        Ok(Self)
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        if let Err(error) = unsafe { MFShutdown() } {
            tracing::error!(%error, "Media Foundation shutdown failed");
        }
    }
}
