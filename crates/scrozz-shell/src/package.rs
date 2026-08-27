//! Runtime package identity and package-owned Windows integration.
//!
//! An MSIX install and a portable executable are deliberately different
//! products from Windows' point of view. Package manifests own protocols and
//! startup tasks; writing parallel registry entries from a packaged process
//! leaves stale state behind after uninstall. This module is the one runtime
//! probe that decides which ownership model applies.

use scrozz_core::{Error, Result};

use crate::RegistrationStatus;

/// How the running executable was deployed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    /// A normal native install on a platform without Windows package identity.
    Native,
    /// An unpackaged Windows executable, normally distributed in the ZIP.
    Portable,
    /// A Windows MSIX package with package identity.
    Msix,
}

impl PackageKind {
    /// The stable token used in capability reports.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Portable => "portable",
            Self::Msix => "msix",
        }
    }

    /// Whether Windows manifest extensions own registration for this process.
    #[must_use]
    pub const fn is_msix(self) -> bool {
        matches!(self, Self::Msix)
    }
}

/// Detects the current process' package identity.
///
/// # Errors
///
/// Returns a platform error if Windows' package identity API returns anything
/// other than "packaged" or its documented "no package" result.
pub fn package_kind() -> Result<PackageKind> {
    package_kind_for_platform()
}

#[cfg(not(target_os = "windows"))]
fn package_kind_for_platform() -> Result<PackageKind> {
    Ok(PackageKind::Native)
}

#[cfg(target_os = "windows")]
fn package_kind_for_platform() -> Result<PackageKind> {
    use windows::Win32::{
        Foundation::{APPMODEL_ERROR_NO_PACKAGE, ERROR_INSUFFICIENT_BUFFER},
        Storage::Packaging::Appx::GetCurrentPackageFullName,
    };

    let mut length = 0;
    // SAFETY: A null buffer with a zero length is the documented size probe.
    // The function writes only the stack-owned length value.
    let status = unsafe { GetCurrentPackageFullName(&mut length, None) };
    classify_package_probe(
        status.0,
        length,
        APPMODEL_ERROR_NO_PACKAGE.0,
        ERROR_INSUFFICIENT_BUFFER.0,
    )
}

#[cfg(any(test, target_os = "windows"))]
fn classify_package_probe(
    status: u32,
    length: u32,
    no_package: u32,
    insufficient_buffer: u32,
) -> Result<PackageKind> {
    if status == no_package {
        Ok(PackageKind::Portable)
    } else if length > 0 && (status == insufficient_buffer || status == 0) {
        Ok(PackageKind::Msix)
    } else {
        Err(Error::Platform(format!(
            "GetCurrentPackageFullName failed with Win32 error {status}"
        )))
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn startup_task_status(task_id: &str) -> Result<RegistrationStatus> {
    use windows::{ApplicationModel::StartupTask, core::HSTRING};

    let operation = StartupTask::GetAsync(&HSTRING::from(task_id)).map_err(|error| {
        Error::Platform(format!(
            "Windows could not find MSIX startup task {task_id:?}: {error}"
        ))
    })?;
    wait_for_completion(
        || operation.Status().map(|status| status.0),
        || operation.Cancel(),
        "look up the MSIX startup task",
    )?;
    let task = operation.GetResults().map_err(|error| {
        Error::Platform(format!(
            "Windows could not load MSIX startup task {task_id:?}: {error}"
        ))
    })?;
    registration_status(task.State().map_err(|error| {
        Error::Platform(format!(
            "Windows could not inspect MSIX startup task {task_id:?}: {error}"
        ))
    })?)
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn startup_task_status(_task_id: &str) -> Result<RegistrationStatus> {
    Err(Error::Unsupported {
        what: "MSIX startup task".to_owned(),
        why: "MSIX startup tasks exist only on Windows".to_owned(),
    })
}

#[cfg(target_os = "windows")]
pub(crate) fn set_startup_task_enabled(task_id: &str, enabled: bool) -> Result<()> {
    use windows::{
        ApplicationModel::{StartupTask, StartupTaskState},
        core::HSTRING,
    };

    let lookup = StartupTask::GetAsync(&HSTRING::from(task_id)).map_err(|error| {
        Error::Platform(format!(
            "Windows could not find MSIX startup task {task_id:?}: {error}"
        ))
    })?;
    wait_for_completion(
        || lookup.Status().map(|status| status.0),
        || lookup.Cancel(),
        "look up the MSIX startup task",
    )?;
    let task = lookup.GetResults().map_err(|error| {
        Error::Platform(format!(
            "Windows could not load MSIX startup task {task_id:?}: {error}"
        ))
    })?;

    if enabled {
        match task.State().map_err(|error| {
            Error::Platform(format!(
                "Windows could not inspect MSIX startup task {task_id:?}: {error}"
            ))
        })? {
            StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy => return Ok(()),
            StartupTaskState::DisabledByPolicy => {
                return Err(Error::PermissionDenied {
                    capability: "launch at login".to_owned(),
                    remedy: "Windows policy disables Scrozz's startup task".to_owned(),
                });
            }
            _ => {}
        }

        let request = task.RequestEnableAsync().map_err(|error| {
            Error::Platform(format!(
                "Windows could not request enabling MSIX startup task {task_id:?}: {error}"
            ))
        })?;
        wait_for_completion(
            || request.Status().map(|status| status.0),
            || request.Cancel(),
            "enable the MSIX startup task",
        )?;
        let state = request.GetResults().map_err(|error| {
            Error::Platform(format!(
                "Windows rejected enabling MSIX startup task {task_id:?}: {error}"
            ))
        })?;
        return match state {
            StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy => Ok(()),
            StartupTaskState::DisabledByUser => Err(Error::PermissionDenied {
                capability: "launch at login".to_owned(),
                remedy: format!("enable {task_id} for Scrozz in Windows Settings > Apps > Startup"),
            }),
            StartupTaskState::DisabledByPolicy => Err(Error::PermissionDenied {
                capability: "launch at login".to_owned(),
                remedy: "Windows policy disables Scrozz's startup task".to_owned(),
            }),
            state => Err(Error::Platform(format!(
                "Windows left MSIX startup task {task_id:?} in unexpected state {}",
                state.0
            ))),
        };
    }

    match task.State().map_err(|error| {
        Error::Platform(format!(
            "Windows could not inspect MSIX startup task {task_id:?}: {error}"
        ))
    })? {
        StartupTaskState::Disabled
        | StartupTaskState::DisabledByUser
        | StartupTaskState::DisabledByPolicy => return Ok(()),
        StartupTaskState::EnabledByPolicy => {
            return Err(Error::PermissionDenied {
                capability: "launch at login".to_owned(),
                remedy: "Windows policy forces Scrozz's startup task on".to_owned(),
            });
        }
        StartupTaskState::Enabled => {}
        state => {
            return Err(Error::Platform(format!(
                "Windows returned unknown startup task state {}",
                state.0
            )));
        }
    }

    task.Disable().map_err(|error| {
        Error::Platform(format!(
            "Windows could not disable MSIX startup task {task_id:?}: {error}"
        ))
    })?;
    match task.State().map_err(|error| {
        Error::Platform(format!(
            "Windows could not verify MSIX startup task {task_id:?}: {error}"
        ))
    })? {
        StartupTaskState::Disabled | StartupTaskState::DisabledByUser => Ok(()),
        state => Err(Error::Platform(format!(
            "Windows left MSIX startup task {task_id:?} in unexpected state {}",
            state.0
        ))),
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn set_startup_task_enabled(_task_id: &str, _enabled: bool) -> Result<()> {
    Err(Error::Unsupported {
        what: "MSIX startup task".to_owned(),
        why: "MSIX startup tasks exist only on Windows".to_owned(),
    })
}

#[cfg(target_os = "windows")]
fn registration_status(
    state: windows::ApplicationModel::StartupTaskState,
) -> Result<RegistrationStatus> {
    use windows::ApplicationModel::StartupTaskState;

    match state {
        StartupTaskState::Enabled | StartupTaskState::EnabledByPolicy => {
            Ok(RegistrationStatus::Enabled)
        }
        StartupTaskState::Disabled
        | StartupTaskState::DisabledByUser
        | StartupTaskState::DisabledByPolicy => Ok(RegistrationStatus::Disabled),
        state => Err(Error::Platform(format!(
            "Windows returned unknown startup task state {}",
            state.0
        ))),
    }
}

#[cfg(target_os = "windows")]
fn wait_for_completion(
    mut status: impl FnMut() -> windows::core::Result<i32>,
    mut cancel: impl FnMut() -> windows::core::Result<()>,
    purpose: &str,
) -> Result<()> {
    use std::time::{Duration, Instant};

    const ASYNC_STARTED: i32 = 0;
    const ASYNC_COMPLETED: i32 = 1;
    const ASYNC_CANCELED: i32 = 2;
    const TIMEOUT: Duration = Duration::from_secs(20);

    let deadline = Instant::now() + TIMEOUT;
    let mut backoff = Duration::from_micros(200);
    loop {
        match status()
            .map_err(|error| Error::Platform(format!("Windows could not {purpose}: {error}")))?
        {
            ASYNC_COMPLETED => return Ok(()),
            ASYNC_CANCELED => return Err(Error::Cancelled),
            ASYNC_STARTED => {}
            // The operation's GetResults call carries the useful HRESULT.
            _ => return Ok(()),
        }
        if Instant::now() >= deadline {
            let _ = cancel();
            return Err(Error::Platform(format!(
                "Windows did not {purpose} within {TIMEOUT:?}"
            )));
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(4));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_kind_tokens_are_stable() {
        assert_eq!(PackageKind::Native.slug(), "native");
        assert_eq!(PackageKind::Portable.slug(), "portable");
        assert_eq!(PackageKind::Msix.slug(), "msix");
        assert!(!PackageKind::Native.is_msix());
        assert!(!PackageKind::Portable.is_msix());
        assert!(PackageKind::Msix.is_msix());
    }

    #[test]
    fn non_windows_hosts_are_native_packages() {
        if !cfg!(target_os = "windows") {
            assert_eq!(package_kind().unwrap(), PackageKind::Native);
        }
    }

    #[test]
    fn package_probe_distinguishes_identity_from_portable_execution() {
        const NO_PACKAGE: u32 = 15_700;
        const INSUFFICIENT_BUFFER: u32 = 122;

        assert_eq!(
            classify_package_probe(NO_PACKAGE, 0, NO_PACKAGE, INSUFFICIENT_BUFFER).unwrap(),
            PackageKind::Portable
        );
        assert_eq!(
            classify_package_probe(INSUFFICIENT_BUFFER, 42, NO_PACKAGE, INSUFFICIENT_BUFFER)
                .unwrap(),
            PackageKind::Msix
        );
        assert_eq!(
            classify_package_probe(0, 42, NO_PACKAGE, INSUFFICIENT_BUFFER).unwrap(),
            PackageKind::Msix
        );
        assert!(
            classify_package_probe(INSUFFICIENT_BUFFER, 0, NO_PACKAGE, INSUFFICIENT_BUFFER)
                .is_err()
        );
        assert!(classify_package_probe(5, 0, NO_PACKAGE, INSUFFICIENT_BUFFER).is_err());
    }
}
