//! Runtime package identity for the current Windows process.

use windows::{
    Win32::{Foundation::ERROR_SUCCESS, Storage::Packaging::Appx::GetCurrentPackageFullName},
    core::PWSTR,
};

use crate::win32::{
    PackageIdentityProbe, WIN32_ERROR_INSUFFICIENT_BUFFER, classify_package_identity_probe,
};

/// Package identity assigned to the running process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageIdentity {
    /// The process has MSIX or sparse-package identity.
    Packaged {
        /// Full package name returned by Windows.
        full_name: String,
    },
    /// A portable, unpackaged Win32 process.
    Unpackaged,
    /// Windows could not determine the process identity.
    Unknown {
        /// Native status from `GetCurrentPackageFullName`.
        status: u32,
        /// Human-readable diagnostic.
        detail: String,
    },
}

impl PackageIdentity {
    /// Stable token for diagnostics.
    #[must_use]
    pub const fn state(&self) -> &'static str {
        match self {
            Self::Packaged { .. } => "packaged",
            Self::Unpackaged => "unpackaged",
            Self::Unknown { .. } => "unknown",
        }
    }
}

/// Detects package identity for the calling process without requiring COM.
#[must_use]
pub fn current() -> PackageIdentity {
    let mut utf16_len = 0_u32;
    let first = unsafe { GetCurrentPackageFullName(&mut utf16_len, None) };
    match classify_package_identity_probe(first.0, utf16_len) {
        PackageIdentityProbe::Unpackaged => PackageIdentity::Unpackaged,
        PackageIdentityProbe::Failed(status) => PackageIdentity::Unknown {
            status,
            detail: format!(
                "GetCurrentPackageFullName sizing probe returned Win32 status {status}"
            ),
        },
        PackageIdentityProbe::Packaged { utf16_len } => read_full_name(utf16_len),
    }
}

fn read_full_name(utf16_len: u32) -> PackageIdentity {
    let mut buffer = vec![0_u16; utf16_len as usize];
    let mut actual_len = utf16_len;
    let status = unsafe {
        GetCurrentPackageFullName(&mut actual_len, Some(PWSTR::from_raw(buffer.as_mut_ptr())))
    };
    if status != ERROR_SUCCESS {
        return PackageIdentity::Unknown {
            status: status.0,
            detail: format!(
                "GetCurrentPackageFullName returned Win32 status {} after requesting \
                 {utf16_len} UTF-16 code units",
                status.0
            ),
        };
    }

    let actual_len = actual_len as usize;
    if actual_len == 0 || actual_len > buffer.len() {
        return PackageIdentity::Unknown {
            status: WIN32_ERROR_INSUFFICIENT_BUFFER,
            detail: format!(
                "GetCurrentPackageFullName reported invalid length {actual_len} for a {}-unit buffer",
                buffer.len()
            ),
        };
    }
    let text_len = actual_len - usize::from(buffer[actual_len - 1] == 0);
    match String::from_utf16(&buffer[..text_len]) {
        Ok(full_name) if !full_name.is_empty() => PackageIdentity::Packaged { full_name },
        Ok(_) => PackageIdentity::Unknown {
            status: 0,
            detail: "GetCurrentPackageFullName returned an empty package name".to_owned(),
        },
        Err(error) => PackageIdentity::Unknown {
            status: 0,
            detail: format!("package full name was not valid UTF-16: {error}"),
        },
    }
}
