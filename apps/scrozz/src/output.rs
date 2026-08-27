//! The one path from encoded capture bytes to a user-visible file.
//!
//! Keeping this shared by CLI and GUI is not tidiness; it prevents data loss.
//! Both previously built names from whole-second timestamps and called
//! `std::fs::write`, so a second save in the same second silently truncated the
//! first. [`FileExporter`] already solves the race with `File::create_new` and
//! numbered collision suffixes. Every default save goes through it.

use std::path::{Path, PathBuf};

use scrozz_core::{Error as CoreError, Frame};
use scrozz_export::{
    Destination, ExportOutcome, FileExporter, ImageFormat, NameTemplate, NamingContext,
};

use crate::fault::{CliError, CliResult};

/// Saves encoded image bytes in the configured/default capture directory.
///
/// # Errors
///
/// Returns a codec error when `bytes` are not PNG, JPEG, or WebP, and a storage
/// or I/O error when no safe path can be created.
pub fn export_default(bytes: &[u8]) -> CliResult<PathBuf> {
    export_to_directory(bytes, &default_directory(), &NamingContext::now())
}

/// Encodes and exports a frame according to the destination's capabilities.
///
/// # Errors
///
/// Returns an export error when the destination cannot accept the frame or the
/// selected encoder/delivery mechanism fails.
pub fn export_frame_auto(frame: &Frame, destination: &Destination) -> CliResult<ExportOutcome> {
    if matches!(destination, Destination::S3 { .. }) {
        return Err(CliError::Core(CoreError::Unsupported {
            what: "automatic editor upload".to_owned(),
            why: "the editor currently exposes clipboard and folder destinations".to_owned(),
        }));
    }
    let template = NameTemplate::parse("Scrozz {date} at {time}")?;
    Ok(FileExporter::new()
        .with_template(template)
        .export_frame(
            frame,
            ImageFormat::Png,
            destination,
            &NamingContext::now(),
        )?)
}

/// The fallback until the save-folder setting is wired into the settings UI.
#[must_use]
pub fn default_directory() -> PathBuf {
    dirs::picture_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
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
}
