//! The one path from encoded capture bytes to a user-visible file.
//!
//! Keeping this shared by CLI and GUI is not tidiness; it prevents data loss.
//! Both previously built names from whole-second timestamps and called
//! `std::fs::write`, so a second save in the same second silently truncated the
//! first. [`FileExporter`] already solves the race with `File::create_new` and
//! numbered collision suffixes. Every default save goes through it.

use std::path::{Path, PathBuf};

use scrozz_core::Error as CoreError;
use scrozz_export::{Destination, FileExporter, NameTemplate, NamingContext};

use crate::fault::{CliError, CliResult};
use crate::{after_capture::AfterCaptureSettings, shortcuts::Shortcuts};

/// Saves encoded image bytes in the configured/default capture directory.
///
/// # Errors
///
/// Returns a codec error when `bytes` are not PNG, JPEG, or WebP, and a storage
/// or I/O error when no safe path can be created.
pub fn export_default(bytes: &[u8]) -> CliResult<PathBuf> {
    let (persisted, _) = crate::settings::stored_settings()?;
    export_with_settings(bytes, &persisted)
}

/// Saves encoded bytes using the exact settings snapshot that started an action
/// pass.
///
/// # Errors
///
/// Returns a settings, codec, storage, or I/O error.
pub fn export_with_settings(bytes: &[u8], persisted: &AfterCaptureSettings) -> CliResult<PathBuf> {
    export_to_directory(bytes, &directory_from(persisted)?, &NamingContext::now())
}

/// The configured capture folder, with a leading home shorthand expanded.
///
/// # Errors
///
/// Returns a settings error when the persisted document is unreadable.
pub fn default_directory() -> CliResult<PathBuf> {
    let (persisted, _) = crate::settings::stored_settings()?;
    directory_from(&persisted)
}

fn directory_from(persisted: &AfterCaptureSettings) -> CliResult<PathBuf> {
    let configured = crate::settings::lookup("capture.folder")?;
    let (value, _) = crate::settings::resolve(configured, &Shortcuts::default(), persisted);
    Ok(expand_home(&value))
}

fn expand_home(value: &str) -> PathBuf {
    if value == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(value)
}

fn export_to_directory(
    bytes: &[u8],
    directory: &Path,
    context: &NamingContext,
) -> CliResult<PathBuf> {
    let template = NameTemplate::parse("Scrozz {date} at {time}")?;
    let outcome = FileExporter::new().with_template(template).export_bytes(
        bytes,
        &Destination::Folder(directory.to_owned()),
        context,
    )?;
    outcome.path.ok_or_else(|| {
        CliError::Core(CoreError::Storage(
            "the folder exporter succeeded without returning a path".to_owned(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use scrozz_export::Timestamp;

    use super::*;

    fn scratch() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("scrozz-output-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn two_saves_in_the_same_second_never_overwrite() {
        let directory = scratch();
        let context = NamingContext {
            timestamp: Some(Timestamp::new(2026, 8, 27, 2, 30, 0)),
            ..NamingContext::default()
        };
        // Folder delivery only needs a trustworthy format signature; the
        // exporter does not decode bytes it is writing unchanged.
        let first_bytes = b"\x89PNG\r\n\x1a\nfirst capture";
        let second_bytes = b"\x89PNG\r\n\x1a\nsecond capture";

        let first = export_to_directory(first_bytes, &directory, &context).expect("first save");
        let second = export_to_directory(second_bytes, &directory, &context).expect("second save");

        assert_ne!(first, second, "colliding saves need distinct paths");
        assert_eq!(std::fs::read(first).unwrap(), first_bytes);
        assert_eq!(std::fs::read(second).unwrap(), second_bytes);

        std::fs::remove_dir_all(directory).expect("clean scratch directory");
    }

    #[test]
    fn automatic_save_uses_the_configured_location() {
        let root = scratch();
        let configured = root.join("chosen");
        let mut settings = AfterCaptureSettings::fresh();
        settings.set_value("capture.folder", configured.to_string_lossy().into_owned());

        let path = export_with_settings(b"\x89PNG\r\n\x1a\nconfigured", &settings)
            .expect("automatic save");
        assert!(path.starts_with(&configured), "{}", path.display());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\x89PNG\r\n\x1a\nconfigured"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
