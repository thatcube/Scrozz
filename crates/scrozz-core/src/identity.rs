//! Who this application says it is.
//!
//! # Why one module owns every name
//!
//! Five separate subsystems have to agree on the same handful of strings, and
//! each of them fails *silently* when they drift:
//!
//! - **The macOS bundle** signs itself with a bundle identifier. TCC attaches
//!   the Screen Recording grant to that identifier, so a mismatch does not
//!   error — it quietly asks the user for permission again.
//! - **The URL scheme registration** claims `scrozz://` on all three platforms.
//!   Register one spelling and handle another and the browser opens nothing.
//! - **Launch at login** writes a launch-agent label, a registry value name or
//!   a `.desktop` file name. Enable with one and disable with another and the
//!   entry is orphaned: the user turns the setting off and Scrozz still starts.
//! - **The updater** asks a manifest for the artefact matching this build, and
//!   a wrong platform key means "no update available" forever.
//! - **Single-instance IPC** names an endpoint per user, per product.
//!
//! None of those produces a stack trace when it goes wrong. They produce a
//! permission prompt that will not stick, a dead link, a setting that does
//! nothing, and an app that never updates. So the strings live here once, and
//! everything downstream reads them rather than repeating them.
//!
//! # Why this is in `scrozz-core` and contains no `cfg(target_os)`
//!
//! `docs/platforms.md` reserves `cfg(target_os)` for the four platform crates,
//! and this module honours that literally: the platform key is derived from
//! [`std::env::consts`], which the compiler resolves for the target anyway.
//! That is not a technicality — it is what lets the updater, the capability
//! report and the packaging hooks be *tested for all three platforms from one
//! machine* instead of only for the host.

/// The product name, as a human sees it.
///
/// Used for the macOS bundle name, the Windows registry key, the Linux
/// `.desktop` `Name=`, and every notification title. Capitalised; the
/// lowercase form is [`EXECUTABLE_STEM`].
pub const PRODUCT_NAME: &str = "Scrozz";

/// The reverse-DNS application identifier.
///
/// This is the string macOS TCC keys permission grants by. Changing it costs
/// every existing user their Screen Recording approval, so it is effectively
/// permanent.
pub const BUNDLE_ID: &str = "com.thatcube.Scrozz";

/// The URL scheme Scrozz claims for automation, without `://`.
///
/// Per NEW-15 the handler is registered but **inert until the user turns the
/// scheme on**. Registration and consent are deliberately separate: an
/// installer may legitimately register a handler, but only the user may decide
/// that a web page is allowed to drive their screenshot tool.
pub const URL_SCHEME: &str = "scrozz";

/// The executable's name without any platform extension.
pub const EXECUTABLE_STEM: &str = "scrozz";

/// The version of this build, from the workspace version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the source lives. Shown in `--json` metadata and in update prompts,
/// so a user can see what they are about to install.
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// The label used for the macOS launch agent and the Windows autostart value.
///
/// Distinct from [`BUNDLE_ID`] only in that it must survive being a filename;
/// it is the same string because reverse-DNS is legal in both places, and one
/// string is one thing to get wrong.
pub const AUTOSTART_LABEL: &str = BUNDLE_ID;

/// The package-relative application id used by the Windows MSIX manifest.
///
/// This becomes part of the app's AUMID after the first Store release and must
/// therefore remain stable.
pub const WINDOWS_APPLICATION_ID: &str = PRODUCT_NAME;

/// The id of the default-off startup task declared by the Windows MSIX package.
///
/// Packaged builds use `Windows.ApplicationModel.StartupTask` to mutate this
/// task. Portable builds continue to own a per-user Run-key value instead.
pub const WINDOWS_STARTUP_TASK_ID: &str = "ScrozzStartup";

/// The operating system this build targets: `macos`, `windows` or `linux`.
///
/// From [`std::env::consts::OS`], so it is correct for the *target* even when
/// the code is cross-checked from another machine.
#[must_use]
pub const fn os() -> &'static str {
    std::env::consts::OS
}

/// The CPU architecture this build targets: `aarch64`, `x86_64`, …
#[must_use]
pub const fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// The key an update manifest uses to name the artefact for this build.
///
/// `os-arch`, e.g. `macos-aarch64`, `windows-x86_64`, `linux-x86_64`. Lowercase
/// and hyphenated so it survives being a filename, a JSON key and a URL path
/// segment unquoted.
///
/// A build whose key is absent from a manifest gets "no update for this
/// platform" — which must be an explicit, reported outcome and never a silent
/// "you are up to date".
#[must_use]
pub fn platform_key() -> String {
    format!("{}-{}", os(), arch())
}

/// The executable's file name on this platform.
#[must_use]
pub fn executable_file_name() -> String {
    if os() == "windows" {
        format!("{EXECUTABLE_STEM}.exe")
    } else {
        EXECUTABLE_STEM.to_string()
    }
}

/// The `User-Agent` sent when checking for updates.
///
/// Deliberately carries only what a static file server needs to serve the right
/// artefact — product, version, platform key. No machine identifier, no install
/// id, no counter: an update check is the one network request Scrozz makes, and
/// it must not become a telemetry channel by accident.
#[must_use]
pub fn user_agent() -> String {
    format!("{PRODUCT_NAME}/{VERSION} ({})", platform_key())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identifier_is_reverse_dns_and_carries_the_product_name() {
        assert!(BUNDLE_ID.starts_with("com."), "{BUNDLE_ID}");
        assert!(BUNDLE_ID.ends_with(PRODUCT_NAME), "{BUNDLE_ID}");
        assert_eq!(BUNDLE_ID.split('.').count(), 3, "{BUNDLE_ID}");
    }

    #[test]
    fn the_scheme_is_a_bare_lowercase_word() {
        // A scheme with punctuation or a `://` suffix is registered on one
        // platform and rejected on another, and the failure is a link that
        // opens nothing rather than an error.
        assert!(
            URL_SCHEME.chars().all(|c| c.is_ascii_lowercase()),
            "{URL_SCHEME}"
        );
        assert!(!URL_SCHEME.contains(':'), "{URL_SCHEME}");
        assert_eq!(URL_SCHEME, EXECUTABLE_STEM);
    }

    #[test]
    fn windows_package_ids_are_stable_manifest_tokens() {
        assert_eq!(WINDOWS_APPLICATION_ID, "Scrozz");
        assert_eq!(WINDOWS_STARTUP_TASK_ID, "ScrozzStartup");
        for value in [WINDOWS_APPLICATION_ID, WINDOWS_STARTUP_TASK_ID] {
            assert!(
                value
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric()),
                "{value}"
            );
        }
    }

    #[test]
    fn the_platform_key_is_two_lowercase_segments() {
        let key = platform_key();
        let segments: Vec<&str> = key.split('-').collect();
        assert_eq!(segments.len(), 2, "{key}");
        assert!(
            key.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{key}"
        );
        assert!(key.starts_with(os()), "{key}");
        assert!(key.ends_with(arch()), "{key}");
    }

    #[test]
    fn the_executable_name_gains_an_extension_only_on_windows() {
        let name = executable_file_name();
        if os() == "windows" {
            assert_eq!(name, "scrozz.exe");
        } else {
            assert_eq!(name, "scrozz");
        }
    }

    #[test]
    fn the_user_agent_carries_no_machine_identity() {
        let agent = user_agent();
        assert!(agent.starts_with("Scrozz/"), "{agent}");
        assert!(agent.contains(VERSION), "{agent}");
        assert!(agent.contains(&platform_key()), "{agent}");
        // The whole string is three known-constant pieces. If it ever grows a
        // fourth, this length bound fails and somebody has to justify it.
        assert_eq!(
            agent.len(),
            PRODUCT_NAME.len() + 1 + VERSION.len() + 2 + platform_key().len() + 1,
            "{agent}"
        );
    }

    #[test]
    fn the_version_is_a_three_part_number() {
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "{VERSION}");
        for part in parts {
            assert!(part.parse::<u64>().is_ok(), "{VERSION}");
        }
    }
}
