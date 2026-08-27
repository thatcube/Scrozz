//! Platform-neutral decisions used by the native Windows adapters.

/// `S_OK`.
pub const HR_S_OK: i32 = 0;
/// `S_FALSE`.
pub const HR_S_FALSE: i32 = 1;
/// `RPC_E_CHANGED_MODE`.
pub const HR_RPC_E_CHANGED_MODE: i32 = -2_147_417_850;

/// What happened when a thread tried to enter a COM/WinRT apartment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApartmentEntry {
    /// This call entered the apartment and owes a matching uninitialise.
    Entered,
    /// Retry with the other apartment model.
    RetryOtherModel,
    /// Windows refused both usable outcomes.
    Failed(i32),
}

impl ApartmentEntry {
    /// Whether a returned guard owes `RoUninitialize`.
    #[must_use]
    pub const fn owes_uninitialise(self) -> bool {
        matches!(self, Self::Entered)
    }
}

/// Classifies one `RoInitialize` result.
#[must_use]
pub const fn classify_apartment_entry(status: i32) -> ApartmentEntry {
    match status {
        HR_S_OK | HR_S_FALSE => ApartmentEntry::Entered,
        HR_RPC_E_CHANGED_MODE => ApartmentEntry::RetryOtherModel,
        other => ApartmentEntry::Failed(other),
    }
}

/// `ERROR_SUCCESS`.
pub const WIN32_ERROR_SUCCESS: u32 = 0;
/// `ERROR_INSUFFICIENT_BUFFER`.
pub const WIN32_ERROR_INSUFFICIENT_BUFFER: u32 = 122;
/// `APPMODEL_ERROR_NO_PACKAGE`.
pub const WIN32_ERROR_NO_PACKAGE_IDENTITY: u32 = 15_700;

/// Result of the sizing probe for `GetCurrentPackageFullName`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageIdentityProbe {
    /// A package identity exists; allocate this many UTF-16 code units.
    Packaged {
        /// Buffer length including the trailing NUL.
        utf16_len: u32,
    },
    /// The process has no package identity.
    Unpackaged,
    /// Windows returned an unexpected or inconsistent status.
    Failed(u32),
}

/// Classifies the sizing call to `GetCurrentPackageFullName`.
#[must_use]
pub const fn classify_package_identity_probe(status: u32, utf16_len: u32) -> PackageIdentityProbe {
    match status {
        WIN32_ERROR_INSUFFICIENT_BUFFER if utf16_len > 1 => {
            PackageIdentityProbe::Packaged { utf16_len }
        }
        WIN32_ERROR_NO_PACKAGE_IDENTITY => PackageIdentityProbe::Unpackaged,
        other => PackageIdentityProbe::Failed(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_apartment_mode_requires_a_real_retry() {
        let entry = classify_apartment_entry(HR_RPC_E_CHANGED_MODE);
        assert_eq!(entry, ApartmentEntry::RetryOtherModel);
        assert!(!entry.owes_uninitialise());
        assert!(classify_apartment_entry(HR_S_FALSE).owes_uninitialise());
    }

    #[test]
    fn package_identity_probe_distinguishes_unknown_from_unpacked() {
        assert_eq!(
            classify_package_identity_probe(WIN32_ERROR_INSUFFICIENT_BUFFER, 42),
            PackageIdentityProbe::Packaged { utf16_len: 42 }
        );
        assert_eq!(
            classify_package_identity_probe(WIN32_ERROR_NO_PACKAGE_IDENTITY, 0),
            PackageIdentityProbe::Unpackaged
        );
        assert_eq!(
            classify_package_identity_probe(WIN32_ERROR_SUCCESS, 0),
            PackageIdentityProbe::Failed(WIN32_ERROR_SUCCESS)
        );
    }
}
