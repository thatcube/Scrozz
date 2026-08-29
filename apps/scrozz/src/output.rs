//! The one path from encoded capture bytes to a user-visible file.
//!
//! Keeping this shared by CLI and GUI is not tidiness; it prevents data loss.
//! Both previously built names from whole-second timestamps and called
//! `std::fs::write`, so a second save in the same second silently truncated the
//! first. [`FileExporter`] already solves the race with `File::create_new` and
//! numbered collision suffixes. Every default save goes through it.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use scrozz_core::Error as CoreError;
use scrozz_export::{
    Destination, FileExporter, ImageFormat, NamePolicy, NameTemplate, NamingContext,
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

/// A fully written file that is still invisible at its requested destination.
///
/// Publishing first uses the platform's no-replace rename. Filesystems without
/// that operation fall back to an exclusive destination plus a checked copy.
/// Neither path overwrites a file another process created between staging and
/// commit.
#[derive(Debug)]
pub struct StagedFile {
    temporary: Option<PathBuf>,
    target: PublishTarget,
}

#[derive(Debug)]
enum PublishTarget {
    Exact(PathBuf),
    Unique {
        directory: PathBuf,
        extension: &'static str,
        context: NamingContext,
    },
}

impl StagedFile {
    /// Writes bytes beside an exact destination without making that destination
    /// visible yet.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the parent directory or staging file cannot be
    /// created, written, or synchronized.
    pub fn for_path(bytes: &[u8], path: PathBuf) -> CliResult<Self> {
        let directory = destination_directory(&path);
        Self::stage(bytes, &directory, PublishTarget::Exact(path))
    }

    /// Writes bytes beside the default capture folder while deferring the final
    /// collision-free filename until commit.
    ///
    /// # Errors
    ///
    /// Returns a codec error for unknown bytes and an I/O error if staging fails.
    pub fn for_default(bytes: &[u8]) -> CliResult<Self> {
        let format = ImageFormat::sniff(bytes).ok_or_else(|| {
            CliError::Core(CoreError::Codec(
                "cannot save: encoded bytes are not PNG, JPEG, or WebP".to_owned(),
            ))
        })?;
        let directory = default_directory();
        Self::stage(
            bytes,
            &directory,
            PublishTarget::Unique {
                directory: directory.clone(),
                extension: format.extension(),
                context: NamingContext::now(),
            },
        )
    }

    fn stage(bytes: &[u8], directory: &Path, target: PublishTarget) -> CliResult<Self> {
        fs::create_dir_all(directory).map_err(|error| CliError::Core(CoreError::Io(error)))?;
        let (temporary, mut file) = create_staging_file(directory)?;
        let write = (|| -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()
        })();
        if let Err(error) = write {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(CliError::Core(CoreError::Io(error)));
        }
        drop(file);
        Ok(Self {
            temporary: Some(temporary),
            target,
        })
    }

    /// Atomically publishes the staged bytes.
    ///
    /// Exact destinations are never overwritten. Default-folder destinations
    /// retry with a numbered filename if another process wins the same name.
    ///
    /// # Errors
    ///
    /// Returns an I/O or naming error without exposing a partial destination.
    pub fn commit(mut self) -> CliResult<PathBuf> {
        let temporary = self
            .temporary
            .as_ref()
            .expect("a staged file always owns its temporary path")
            .clone();
        let mut staging_moved = false;
        let destination = match &self.target {
            PublishTarget::Exact(path) => {
                staging_moved = publish_no_replace(&temporary, path)
                    .map_err(|error| CliError::Core(CoreError::Io(error)))?
                    == PublishDisposition::Moved;
                path.clone()
            }
            PublishTarget::Unique {
                directory,
                extension,
                context,
            } => {
                let template = NameTemplate::parse("Scrozz {date} at {time}")?;
                let policy = NamePolicy::default();
                let mut destination = None;
                for _ in 0..64 {
                    let candidate = policy.unique_path(
                        directory,
                        &template,
                        context,
                        extension,
                        &mut |path| path.exists(),
                    )?;
                    match publish_no_replace(&temporary, &candidate) {
                        Ok(disposition) => {
                            staging_moved = disposition == PublishDisposition::Moved;
                            destination = Some(candidate);
                            break;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(CliError::Core(CoreError::Io(error))),
                    }
                }
                destination.ok_or_else(|| {
                    CliError::Core(CoreError::Storage(format!(
                        "could not publish a collision-free capture in {} after 64 attempts",
                        directory.display()
                    )))
                })?
            }
        };

        if staging_moved {
            self.temporary.take();
        } else if let Some(temporary) = self.temporary.take()
            && let Err(error) = fs::remove_file(&temporary)
        {
            tracing::warn!(
                path = %temporary.display(),
                %error,
                "published capture but could not remove its hidden staging file"
            );
        }
        Ok(destination)
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if let Some(temporary) = self.temporary.take() {
            let _ = fs::remove_file(temporary);
        }
    }
}

fn destination_directory(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_owned()
}

fn create_staging_file(directory: &Path) -> CliResult<(PathBuf, File)> {
    static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

    for _ in 0..64 {
        let nonce = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".scrozz-stage-{}-{nonce}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(CliError::Core(CoreError::Io(error))),
        }
    }
    Err(CliError::Core(CoreError::Storage(format!(
        "could not create a staging file in {} after 64 attempts",
        directory.display()
    ))))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublishDisposition {
    Moved,
    Copied,
}

fn publish_no_replace(staging: &Path, destination: &Path) -> io::Result<PublishDisposition> {
    match rename_no_replace(staging, destination) {
        Ok(()) => Ok(PublishDisposition::Moved),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(error),
        Err(rename_error) => {
            tracing::debug!(
                source = %staging.display(),
                target = %destination.display(),
                error = %rename_error,
                "no-replace rename unavailable; publishing through an exclusive copy"
            );
            copy_exclusive(staging, destination)?;
            Ok(PublishDisposition::Copied)
        }
    }
}

fn copy_exclusive(staging: &Path, destination: &Path) -> io::Result<()> {
    let mut source = File::open(staging)?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let copied = (|| -> io::Result<()> {
        io::copy(&mut source, &mut target)?;
        target.sync_all()
    })();
    if let Err(error) = copied {
        drop(target);
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{
        ffi::CString,
        os::{raw::c_char, unix::ffi::OsStrExt as _},
    };

    const AT_FDCWD: i32 = -100;
    const RENAME_NOREPLACE: u32 = 1;
    unsafe extern "C" {
        fn renameat2(
            olddirfd: i32,
            oldpath: *const c_char,
            newdirfd: i32,
            newpath: *const c_char,
            flags: u32,
        ) -> i32;
    }

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    // SAFETY: both C strings live through the call; AT_FDCWD addresses their
    // absolute or process-relative paths and RENAME_NOREPLACE forbids overwrite.
    let result = unsafe {
        renameat2(
            AT_FDCWD,
            source.as_ptr(),
            AT_FDCWD,
            destination.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{
        ffi::CString,
        os::{raw::c_char, unix::ffi::OsStrExt as _},
    };

    const RENAME_EXCL: u32 = 0x0000_0004;
    unsafe extern "C" {
        fn renamex_np(old: *const c_char, new: *const c_char, flags: u32) -> i32;
    }

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    // SAFETY: both C strings live through the call and RENAME_EXCL forbids
    // replacing an existing destination.
    let result = unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "windows")]
fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    let source: Vec<u16> = source.as_os_str().encode_wide().chain([0]).collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: both NUL-terminated buffers live through the call. Omitting
    // MOVEFILE_REPLACE_EXISTING gives this operation no-replace semantics.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "windows"
)))]
fn rename_no_replace(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform has no no-replace rename binding",
    ))
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
        static NEXT_SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let sequence = NEXT_SCRATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    fn staged_file_is_invisible_until_commit_and_publishes_complete_bytes() {
        let directory = scratch();
        let destination = directory.join("capture.png");
        let bytes = b"\x89PNG\r\n\x1a\ncomplete capture";

        let staged = StagedFile::for_path(bytes, destination.clone()).expect("stage");
        assert!(!destination.exists());

        assert_eq!(staged.commit().expect("commit"), destination);
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
        assert!(std::fs::read_dir(&directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".scrozz-stage-")
        }));

        std::fs::remove_dir_all(directory).expect("clean scratch directory");
    }

    #[test]
    fn staged_file_never_overwrites_an_existing_destination() {
        let directory = scratch();
        std::fs::create_dir_all(&directory).unwrap();
        let destination = directory.join("capture.png");
        std::fs::write(&destination, b"existing").unwrap();

        let staged =
            StagedFile::for_path(b"\x89PNG\r\n\x1a\nnew", destination.clone()).expect("stage");
        let error = staged.commit().expect_err("must not overwrite");
        assert!(matches!(error, CliError::Core(CoreError::Io(_))));
        assert_eq!(std::fs::read(&destination).unwrap(), b"existing");
        assert!(std::fs::read_dir(&directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".scrozz-stage-")
        }));

        std::fs::remove_dir_all(directory).expect("clean scratch directory");
    }

    #[test]
    fn exclusive_copy_fallback_is_complete_and_never_overwrites() {
        let directory = scratch();
        std::fs::create_dir_all(&directory).unwrap();
        let staging = directory.join(".staged");
        let destination = directory.join("capture.png");
        let bytes = b"\x89PNG\r\n\x1a\nfallback capture";
        std::fs::write(&staging, bytes).unwrap();

        copy_exclusive(&staging, &destination).expect("exclusive copy");
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
        assert_eq!(std::fs::read(&staging).unwrap(), bytes);

        let error = copy_exclusive(&staging, &destination).expect_err("must not overwrite");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);

        std::fs::remove_dir_all(directory).expect("clean scratch directory");
    }
}
