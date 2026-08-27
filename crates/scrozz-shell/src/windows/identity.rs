//! Runtime package identity for Win32 processes.
//!
//! A Windows executable is not inherently packaged just because an MSIX exists
//! somewhere on the machine. Identity belongs to the running process. A sparse
//! package can grant it to an externally installed executable, while the exact
//! same bytes launched from a portable ZIP have none.
//!
//! `Windows.Media.Ocr` is therefore selected from this runtime answer, never
//! from a build flag or an assumed release channel. Capture itself does not need
//! package identity and must continue to work for all three outcomes below.

use windows::{
    Win32::{Foundation::ERROR_SUCCESS, Storage::Packaging::Appx::GetCurrentPackageFullName},
    core::PWSTR,
};

use crate::win32::{
    PackageIdentityProbe, WIN32_ERROR_INSUFFICIENT_BUFFER, classify_package_identity_probe,
};

/// The package identity assigned to the running process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageIdentity {
    /// The process has MSIX or sparse-package identity.
    Packaged {
        /// The full package name returned by Windows.
        full_name: String,
    },
    /// A portable/unpackaged Win32 process.
    Unpackaged,
    /// Windows could not determine the process identity.
    ///
    /// This is distinct from [`Self::Unpackaged`]. Consumers may continue with
    /// capabilities such as screen capture that do not require identity, but
    /// must not guess which identity-sensitive API is safe to call.
    Unknown {
        /// Native status from `GetCurrentPackageFullName`, when available.
        status: u32,
        /// Human-readable diagnostic.
        detail: String,
    },
}

impl PackageIdentity {
    /// Stable token for diagnostics and JSON.
    #[must_use]
    pub const fn state(&self) -> &'static str {
        match self {
            Self::Packaged { .. } => "packaged",
            Self::Unpackaged => "unpackaged",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// The package full name, when the process has identity.
    #[must_use]
    pub fn full_name(&self) -> Option<&str> {
        match self {
            Self::Packaged { full_name } => Some(full_name),
            Self::Unpackaged | Self::Unknown { .. } => None,
        }
    }

    /// Native failure status for an indeterminate identity.
    #[must_use]
    pub const fn failure_status(&self) -> Option<u32> {
        match self {
            Self::Unknown { status, .. } => Some(*status),
            Self::Packaged { .. } | Self::Unpackaged => None,
        }
    }

    /// Diagnostic for an indeterminate identity.
    #[must_use]
    pub fn failure_detail(&self) -> Option<&str> {
        match self {
            Self::Unknown { detail, .. } => Some(detail),
            Self::Packaged { .. } | Self::Unpackaged => None,
        }
    }
}

/// Detects package identity for the calling process.
///
/// This uses the Win32 app-model API rather than `Package::Current`, so the
/// query itself needs neither a COM apartment nor package identity.
#[must_use]
pub fn current() -> PackageIdentity {
    let mut utf16_len = 0u32;
    // SAFETY: the documented sizing call accepts a valid length pointer and a
    // null output buffer. It writes only the length.
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
    let mut buffer = vec![0u16; utf16_len as usize];
    let mut actual_len = utf16_len;
    // SAFETY: `buffer` contains `utf16_len` writable UTF-16 code units and
    // `actual_len` advertises exactly that capacity.
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
                "GetCurrentPackageFullName reported an invalid length {actual_len} \
                 for a {}-unit buffer",
                buffer.len()
            ),
        };
    }

    let text_len = if buffer[actual_len - 1] == 0 {
        actual_len - 1
    } else {
        actual_len
    };
    match String::from_utf16(&buffer[..text_len]) {
        Ok(full_name) if !full_name.is_empty() => PackageIdentity::Packaged { full_name },
        Ok(_) => PackageIdentity::Unknown {
            status: 0,
            detail: "GetCurrentPackageFullName returned an empty package name".to_owned(),
        },
        Err(err) => PackageIdentity::Unknown {
            status: 0,
            detail: format!("package full name was not valid UTF-16: {err}"),
        },
    }
}
