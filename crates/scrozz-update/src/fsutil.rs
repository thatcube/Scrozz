use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{Error, Result};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct ReservedFile {
    path: PathBuf,
    file: File,
}

impl ReservedFile {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    pub(crate) fn into_parts(self) -> (PathBuf, File) {
        (self.path, self.file)
    }
}

pub(crate) fn parent(path: &Path) -> Result<&Path> {
    path.parent()
        .ok_or_else(|| Error::InvalidState(format!("`{}` has no parent", path.display())))
        .map(|value| {
            if value.as_os_str().is_empty() {
                Path::new(".")
            } else {
                value
            }
        })
}

pub(crate) fn absolute(path: &Path, operation: &'static str) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|error| Error::io(operation, path, error))
}

pub(crate) fn ensure_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| Error::io("inspect update file", path, error))?;
    if metadata.is_dir() {
        return Err(Error::DirectoryUnsupported(path.to_path_buf()));
    }
    if !metadata.file_type().is_file() {
        return Err(Error::NotRegularFile(path.to_path_buf()));
    }
    Ok(())
}

pub(crate) fn ensure_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(Error::DestinationExists(path.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io("inspect destination", path, error)),
    }
}

pub(crate) fn reserve_temp(parent: &Path, label: &str) -> Result<ReservedFile> {
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".scrozz-{label}-{}-{sequence}.part",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok(ReservedFile { path, file }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(Error::io("reserve temporary file", path, error)),
        }
    }
    Err(Error::InvalidState(format!(
        "could not reserve a temporary file in `{}`",
        parent.display()
    )))
}

pub(crate) fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Err(Error::DirectoryUnsupported(path.to_path_buf())),
        Ok(_) => fs::remove_file(path)
            .map_err(|error| Error::io("remove temporary update file", path, error)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io("inspect temporary update file", path, error)),
    }
}

pub(crate) fn sync_parent(path: &Path) -> Result<()> {
    let directory_path = parent(path)?;
    let directory = open_directory(directory_path)?;
    directory
        .sync_all()
        .map_err(|error| Error::io("sync parent directory", directory_path, error))
}

pub(crate) fn rename_synced(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).map_err(|error| Error::io("rename update file", from, error))?;
    sync_parent(to)
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = parent(path)?;
    recover_atomic_write(path)?;
    let reserved = reserve_temp(directory, "state")?;
    let (temporary, mut file) = reserved.into_parts();
    let result = (|| {
        file.write_all(bytes)
            .map_err(|error| Error::io("write temporary state", &temporary, error))?;
        file.sync_all()
            .map_err(|error| Error::io("sync temporary state", &temporary, error))?;
        drop(file);

        let backup = backup_path(path)?;
        if path_exists(&backup)? {
            remove_file_if_present(&backup)?;
            sync_parent(path)?;
        }
        if path_exists(path)? {
            ensure_regular_file(path)?;
            rename_synced(path, &backup)?;
        }
        rename_synced(&temporary, path)
    })();
    if result.is_err() {
        let _ = remove_file_if_present(&temporary);
    }
    result
}

/// Restores the previous state if a process stopped between the two renames.
///
/// The backup is retained after each successful replacement. This makes every
/// point in the replacement sequence recoverable without relying on
/// overwrite-on-rename behavior, which differs between Unix and Windows.
pub(crate) fn recover_atomic_write(path: &Path) -> Result<()> {
    let backup = backup_path(path)?;
    let current_exists = path_exists(path)?;
    let backup_exists = path_exists(&backup)?;
    match (current_exists, backup_exists) {
        (false, true) => {
            ensure_regular_file(&backup)?;
            rename_synced(&backup, path)
        }
        (true, _) => {
            ensure_regular_file(path)?;
            if backup_exists {
                ensure_regular_file(&backup)?;
            }
            Ok(())
        }
        (false, false) => Ok(()),
    }
}

pub(crate) fn ensure_distinct_siblings(paths: &[&Path]) -> Result<()> {
    let Some((first, rest)) = paths.split_first() else {
        return Ok(());
    };
    let directory = parent(first)?;
    for (index, path) in paths.iter().enumerate() {
        if parent(path)? != directory || paths[..index].contains(path) {
            return Err(Error::PathsNotSiblings);
        }
    }
    debug_assert_eq!(rest.len() + 1, paths.len());
    Ok(())
}

fn backup_path(path: &Path) -> Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        Error::InvalidState(format!("`{}` has no state file name", path.display()))
    })?;
    let mut backup_name = OsString::from(file_name);
    backup_name.push(".previous");
    Ok(parent(path)?.join(backup_name))
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::io("inspect update path", path, error)),
    }
}

#[cfg(unix)]
fn open_directory(path: &Path) -> Result<File> {
    File::open(path).map_err(|error| Error::io("open parent directory", path, error))
}

#[cfg(windows)]
fn open_directory(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .map_err(|error| Error::io("open parent directory", path, error))
}

#[cfg(not(any(unix, windows)))]
fn open_directory(path: &Path) -> Result<File> {
    File::open(path).map_err(|error| Error::io("open parent directory", path, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "scrozz-fsutil-{name}-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn atomic_state_replacement_retains_a_recoverable_previous_file() {
        let root = scratch("replace");
        fs::create_dir_all(&root).unwrap();
        let state = root.join("state.json");
        atomic_write(&state, b"one").unwrap();
        atomic_write(&state, b"two").unwrap();
        assert_eq!(fs::read(&state).unwrap(), b"two");
        assert_eq!(fs::read(backup_path(&state).unwrap()).unwrap(), b"one");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn interrupted_replacement_restores_the_previous_file() {
        let root = scratch("recover");
        fs::create_dir_all(&root).unwrap();
        let state = root.join("state.json");
        let backup = backup_path(&state).unwrap();
        fs::write(&backup, b"durable").unwrap();

        recover_atomic_write(&state).unwrap();

        assert_eq!(fs::read(&state).unwrap(), b"durable");
        assert!(!backup.exists());
        let _ = fs::remove_dir_all(root);
    }
}
