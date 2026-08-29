//! The one path from encoded capture bytes to a user-visible file.
//!
//! Keeping this shared by CLI and GUI is not tidiness; it prevents data loss.
//! Both previously built names from whole-second timestamps and called
//! `std::fs::write`, so a second save in the same second silently truncated the
//! first. [`FileExporter`] already solves the race with `File::create_new` and
//! numbered collision suffixes. Every default save goes through it.

use std::path::{Path, PathBuf};

use scrozz_core::Error as CoreError;
use scrozz_export::{Destination, FileExporter, NamePolicy, NameTemplate, NamingContext};

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

/// A fresh, unused absolute path for a recording about to start.
///
/// **Durable by construction.** The file lives in the user's configured capture
/// folder, not in a temporary directory, because the finished video outlives
/// both the recorder's own scratch space and the card that shows it. Nothing
/// downstream — card dismiss, history deletion, temp sweeping — may remove it.
///
/// **The file is deliberately not created here.** Every recording engine opens
/// its destination with `create_new` and refuses a path that already exists —
/// macOS, Media Foundation and the Linux muxer all do, and that refusal is what
/// stops a recording silently overwriting something. Reserving the name by
/// touching the file would therefore make every default-destination recording
/// fail, and would leave an empty `.mp4` behind on every cancelled selection.
/// The engine's own atomic create is the winner of any race; this only has to
/// pick a name that is free right now.
///
/// # Errors
///
/// Returns a settings error when the persisted document is unreadable, and a
/// storage error when the folder cannot be made or no free name exists.
pub fn default_recording_path() -> CliResult<PathBuf> {
    let (persisted, _) = crate::settings::stored_settings()?;
    reserve_recording_path(&directory_from(&persisted)?, &NamingContext::now())
}

fn reserve_recording_path(directory: &Path, context: &NamingContext) -> CliResult<PathBuf> {
    std::fs::create_dir_all(directory).map_err(|error| {
        CliError::Core(CoreError::Storage(format!(
            "could not make the recording folder {}: {error}",
            directory.display()
        )))
    })?;
    // Canonicalised now rather than after the recording, because history stores
    // the path and requires a canonical one, and the directory is the only part
    // that exists yet.
    let directory = std::fs::canonicalize(directory).map_err(|error| {
        CliError::Core(CoreError::Storage(format!(
            "could not resolve the recording folder {}: {error}",
            directory.display()
        )))
    })?;
    let policy = NamePolicy::default();
    let template = NameTemplate::parse("Scrozz {date} at {time}")?;
    let stem = policy.sanitise(&template.render(context, &policy));
    for attempt in 0..MAX_RECORDING_NAME_ATTEMPTS {
        let name = if attempt == 0 {
            format!("{stem}.mp4")
        } else {
            format!("{stem} ({attempt}).mp4")
        };
        let candidate = directory.join(name);
        match candidate.try_exists() {
            Ok(false) => return Ok(candidate),
            Ok(true) => {}
            Err(error) => {
                return Err(CliError::Core(CoreError::Storage(format!(
                    "could not check the recording destination {}: {error}",
                    candidate.display()
                ))));
            }
        }
    }
    Err(CliError::Core(CoreError::Storage(format!(
        "no free recording name was available in {} after {MAX_RECORDING_NAME_ATTEMPTS} attempts",
        directory.display()
    ))))
}

/// How many numbered suffixes a reservation tries before giving up.
const MAX_RECORDING_NAME_ATTEMPTS: u32 = 1_000;

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use scrozz_export::Timestamp;

    use super::*;

    /// A directory no other test in this process can be handed.
    ///
    /// The nonce alone is not enough: `SystemTime` is coarser than a nanosecond
    /// on some platforms, and these tests run in parallel, so two of them can
    /// read the same instant and then delete each other's scratch directory.
    /// The sequence makes the answer unique by construction.
    fn scratch() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "scrozz-output-{}-{nonce}-{sequence}",
            std::process::id()
        ))
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
    fn a_recording_destination_is_free_and_is_never_created_in_advance() {
        let directory = scratch();
        let context = NamingContext {
            timestamp: Some(Timestamp::new(2026, 8, 29, 9, 15, 0)),
            ..NamingContext::default()
        };

        let first = reserve_recording_path(&directory, &context).expect("first destination");
        assert!(first.is_absolute());
        assert_eq!(first.extension().and_then(|e| e.to_str()), Some("mp4"));
        assert!(
            !first.exists(),
            "every recording engine opens its destination with create_new and \
             refuses a path that already exists, so nothing may be written here yet"
        );

        // Nothing was created, so the same second legitimately yields the same
        // name again; the engine's own atomic create resolves any real race.
        let again = reserve_recording_path(&directory, &context).expect("second destination");
        assert_eq!(first, again);

        // Once a file really is there, the next name steps around it.
        std::fs::write(&first, b"recorded").expect("finished recording");
        let next = reserve_recording_path(&directory, &context).expect("third destination");
        assert_ne!(next, first);
        assert!(!next.exists());

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
