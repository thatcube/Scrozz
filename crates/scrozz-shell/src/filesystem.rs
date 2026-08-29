//! Platform-correct atomic file replacement.

use std::path::Path;

use scrozz_core::{Error, Result};

/// Atomically replaces `destination` with the complete sibling `source` file.
///
/// # Errors
///
/// Returns a storage error when the platform cannot replace the destination.
pub fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    replace_platform(source, destination).map_err(|error| {
        Error::Storage(format!(
            "could not replace {} with {}: {error}",
            destination.display(),
            source.display()
        ))
    })
}

#[cfg(not(target_os = "windows"))]
fn replace_platform(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(target_os = "windows")]
fn replace_platform(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::{iter, os::windows::ffi::OsStrExt as _};
    use windows::{
        Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        core::PCWSTR,
    };

    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    // SAFETY: both pointers reference NUL-terminated UTF-16 buffers that remain
    // alive for the call, and the flags request an atomic same-volume replace.
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|error| std::io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_overwrites_an_existing_complete_file() {
        let root =
            std::env::temp_dir().join(format!("scrozz-atomic-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("new.tmp");
        let destination = root.join("settings.json");
        std::fs::write(&source, b"new complete document").unwrap();
        std::fs::write(&destination, b"old complete document").unwrap();

        replace_file(&source, &destination).unwrap();

        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"new complete document"
        );
        assert!(!source.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
