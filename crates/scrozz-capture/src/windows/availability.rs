//! Deciding *why* the WGC path is unavailable, without asking Windows.
//!
//! # Why this is its own module
//!
//! The question "can this machine use `Windows.Graphics.Capture`" has an
//! obvious `bool` answer and a non-obvious set of reasons, and the reasons
//! matter more than the answer.
//!
//! `GraphicsCaptureSession::IsSupported()` returns a `Result<bool>`. On a
//! thread that never entered a COM apartment it returns
//! `Err(CO_E_NOTINITIALIZED)` — and the idiom that grows around such a call,
//! `.unwrap_or(false)`, reads that as *this machine cannot capture*. Scrozz
//! then takes the GDI fallback, silently losing cursor control, per-window
//! capture and alpha, on hardware where WGC works fine. Nothing is logged,
//! because from the code's point of view nothing failed.
//!
//! Naming that case separately is the entire fix, and naming it in a module
//! that mentions no `windows` type means the naming can be tested on the
//! machine the code is actually written on.

/// `CO_E_NOTINITIALIZED` — the calling thread is in no COM apartment.
///
/// Restated here rather than imported so this module compiles on any host.
/// `scrozz_shell::win32::HR_CO_E_NOTINITIALIZED` is the same value, and the
/// Windows build asserts the two agree.
pub const CO_E_NOTINITIALIZED: i32 = 0x8004_01F0_u32 as i32;

/// Whether the WGC path can be used, and if not, why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WgcAvailability {
    /// WGC can be used on this thread right now.
    Available,
    /// The OS genuinely does not offer it — pre-1803 Windows, or a locked-down
    /// SKU. Falling back to GDI is the *correct* response, not a downgrade.
    Unsupported,
    /// The calling thread never entered a COM apartment.
    ///
    /// A bug in Scrozz, not a property of the machine. The fix is to enter an
    /// apartment at the top of the thread that builds the capture backend.
    ApartmentMissing,
    /// Windows refused for some other reason. The code is kept verbatim so a
    /// bug report carries something more useful than "it didn't work".
    Refused(i32),
}

impl WgcAvailability {
    /// Whether the WGC path may be attempted.
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }

    /// Whether falling back to GDI reflects reality rather than hiding a bug.
    ///
    /// The predicate a future "should we warn the user their captures are
    /// degraded?" check wants: a genuinely unsupported machine should be quiet,
    /// and everything else should not be.
    #[must_use]
    pub const fn fallback_is_legitimate(self) -> bool {
        matches!(self, Self::Unsupported)
    }

    /// Whether this outcome indicates a mistake on Scrozz's side.
    #[must_use]
    pub const fn is_our_fault(self) -> bool {
        matches!(self, Self::ApartmentMissing)
    }
}

/// Classifies the outcome of `GraphicsCaptureSession::IsSupported()`.
///
/// Takes the already-unwrapped shape — `Ok(bool)` or `Err(hresult)` — so that
/// no `windows` type appears in the signature and the mapping stays testable
/// off-Windows.
#[must_use]
pub const fn classify(reported: core::result::Result<bool, i32>) -> WgcAvailability {
    match reported {
        Ok(true) => WgcAvailability::Available,
        Ok(false) => WgcAvailability::Unsupported,
        Err(CO_E_NOTINITIALIZED) => WgcAvailability::ApartmentMissing,
        Err(code) => WgcAvailability::Refused(code),
    }
}

/// What to write to the log for an unavailable outcome, or `None` when the
/// answer was yes.
///
/// Returned rather than logged so the wording is a value a test can read. A
/// silent downgrade is the failure mode this whole module exists to prevent, so
/// "there is always a message" is itself worth asserting.
#[must_use]
pub fn explanation(availability: WgcAvailability) -> Option<String> {
    match availability {
        WgcAvailability::Available => None,
        WgcAvailability::Unsupported => Some(
            "Windows.Graphics.Capture is not available on this build of Windows; \
             using the GDI fallback"
                .to_owned(),
        ),
        WgcAvailability::ApartmentMissing => Some(
            "Windows.Graphics.Capture was probed from a thread with no COM \
             apartment, so it reports itself unavailable. This is a Scrozz bug, \
             not a limitation of this machine: the thread that builds the \
             capture backend must enter an apartment first. Falling back to \
             GDI, which loses cursor control and per-window capture"
                .to_owned(),
        ),
        WgcAvailability::Refused(code) => Some(format!(
            "Windows.Graphics.Capture refused the support query (0x{code:08X}); \
             using the GDI fallback"
        )),
    }
}
