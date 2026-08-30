//! OS permission gates, and the exact place a user goes to grant them.
//!
//! Per decision D15 Scrozz **attempts everything and asks at the moment a
//! feature is first used**, never during onboarding. A permission wall before
//! the user has seen the app do anything is the single most common way these
//! tools lose people. So nothing in this module is called at launch: capture
//! calls [`Permissions::is_granted`] when the user actually captures, recording
//! asks for the microphone when the user actually enables audio, and the
//! window-metadata path asks for Accessibility only if it needs a window title.
//!
//! # What each capability actually gates on macOS
//!
//! | Capability | API | What breaks without it |
//! |---|---|---|
//! | Screen Recording | `CGPreflightScreenCaptureAccess` | Captures return desktop wallpaper with no windows — silently, which is why it is preflighted rather than inferred from the image |
//! | Microphone | `AVCaptureDevice.authorizationStatus(for:)` | Screen recordings have no voiceover track |
//! | Accessibility | `AXIsProcessTrustedWithOptions` | No window titles, no synthesised input |
//!
//! # Why `request` opens Settings
//!
//! macOS shows each of these prompts **once per app, ever**. After the first
//! denial the request API returns immediately with no UI at all, so a naive
//! implementation appears to do nothing. [`Permissions::request`] therefore
//! falls back to opening the exact Settings pane through the
//! `x-apple.systempreferences:` URL scheme — landing the user on the right list
//! rather than on the front page of System Settings.

use scrozz_core::{Error, Result};

use crate::{Capability, Permissions};

/// The `x-apple.systempreferences:` URL for the pane that grants a capability.
///
/// These anchors are the ones System Settings has used since Ventura and are
/// stable across Sonoma and Sequoia. They are returned as plain `&str` so the
/// mapping can be tested without a window server.
#[must_use]
pub const fn settings_pane_url(capability: Capability) -> &'static str {
    match capability {
        Capability::ScreenRecording => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        }
        Capability::Microphone => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        }
        Capability::Accessibility => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        }
    }
}

/// The capability's name in the words the OS itself uses.
///
/// Used for [`Error::PermissionDenied::capability`]. "Screen Recording" is
/// capitalised the way System Settings capitalises it so that a user searching
/// for it finds it.
#[must_use]
pub const fn capability_name(capability: Capability) -> &'static str {
    match capability {
        Capability::ScreenRecording => "Screen Recording",
        Capability::Microphone => "Microphone",
        Capability::Accessibility => "Accessibility",
    }
}

/// Where the user grants this capability, in their platform's own words.
///
/// Used for [`Error::PermissionDenied::remedy`]. Names the literal path through
/// System Settings rather than describing it, because the user is going to
/// retype it.
#[must_use]
pub const fn remedy(capability: Capability) -> &'static str {
    if cfg!(target_os = "macos") {
        match capability {
            Capability::ScreenRecording => {
                "System Settings → Privacy & Security → Screen & System Audio Recording"
            }
            Capability::Microphone => "System Settings → Privacy & Security → Microphone",
            Capability::Accessibility => "System Settings → Privacy & Security → Accessibility",
        }
    } else {
        "your system's privacy settings"
    }
}

/// Builds the [`Error::PermissionDenied`] for a capability.
///
/// Every call site that discovers a missing grant should route through here so
/// that the user-facing wording is identical everywhere.
#[must_use]
pub fn denied(capability: Capability) -> Error {
    Error::PermissionDenied {
        capability: capability_name(capability).to_owned(),
        remedy: remedy(capability).to_owned(),
    }
}

/// The platform's real permission gates.
///
/// A zero-sized handle; there is no state to hold because every query goes
/// straight to the OS. Deliberately *not* cached: the user can revoke a grant
/// in System Settings while Scrozz is running, and a cached `true` would turn
/// that into a stream of blank captures.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemPermissions;

impl SystemPermissions {
    /// Creates a handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Permissions for SystemPermissions {
    fn is_granted(&self, capability: Capability) -> bool {
        #[cfg(test)]
        {
            let _ = capability;
            true
        }
        #[cfg(all(not(test), target_os = "macos"))]
        {
            crate::macos::permissions::is_granted(capability)
        }
        #[cfg(all(not(test), not(target_os = "macos")))]
        {
            // Windows and X11 gate none of these at the API level for the paths
            // Scrozz uses, and Wayland gates capture behind a portal that
            // *is* the request rather than a queryable flag. Reporting "granted"
            // and letting the attempt fail is the D15-consistent answer: it
            // attempts the thing rather than pre-emptively refusing.
            let _ = capability;
            true
        }
    }

    fn request(&self, capability: Capability) -> Result<()> {
        #[cfg(test)]
        {
            tracing::debug!(
                capability = capability_name(capability),
                "permission requests are inert under cfg(test)"
            );
            Ok(())
        }
        #[cfg(all(not(test), target_os = "macos"))]
        {
            crate::macos::permissions::request(capability)
        }
        #[cfg(all(not(test), not(target_os = "macos")))]
        {
            tracing::debug!(
                capability = capability_name(capability),
                "no permission gate to request on this platform"
            );
            Ok(())
        }
    }
}

/// Opens the Settings pane for a capability without querying anything first.
///
/// Exposed separately because the UI wants a "Open Settings" button on the
/// permission-denied surface, and that button must work even when
/// [`Permissions::request`] has already been used up.
///
/// # Errors
///
/// Returns [`Error::Platform`] if the URL could not be opened, and
/// [`Error::Unsupported`] on platforms with no such scheme.
pub fn open_settings(capability: Capability) -> Result<()> {
    #[cfg(test)]
    {
        Err(Error::Unsupported {
            what: format!("opening settings for {}", capability_name(capability)),
            why: "OS launchers are disabled under cfg(test)".to_owned(),
        })
    }
    #[cfg(all(not(test), target_os = "macos"))]
    {
        crate::macos::permissions::open_settings_pane(capability)
    }
    #[cfg(all(not(test), not(target_os = "macos")))]
    {
        Err(Error::Unsupported {
            what: format!("opening settings for {}", capability_name(capability)),
            why: "no settings-pane URL scheme is wired up for this platform yet".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_permission_and_settings_actions_are_inert_under_tests() {
        let permissions = SystemPermissions::new();
        assert!(permissions.is_granted(Capability::ScreenRecording));
        permissions
            .request(Capability::ScreenRecording)
            .expect("test permission requests are inert");
        let error = open_settings(Capability::ScreenRecording)
            .expect_err("test settings launch must be refused");
        assert!(error.to_string().contains("disabled under cfg(test)"));
    }
}
