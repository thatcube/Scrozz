//! The on-disk layout: where the index, the pixels and the documents live.
//!
//! ```text
//! <data dir>/Scrozz/
//! ├── index.sqlite            the query index — rebuildable, never authoritative
//! ├── images/ab/cd/<sha256>   source pixels, content-addressed and deduplicated
//! └── documents/<id>.json     the durable record: metadata + annotations
//! ```
//!
//! # Why image bytes are not in SQLite
//!
//! A screenshot history is tens of thousands of multi-megabyte blobs against a
//! handful of kilobyte-sized documents. Putting the blobs in the database makes
//! every backup, every `VACUUM` and every corruption event scale with the
//! pixels rather than with the metadata, and makes eviction — the one operation
//! decision D23 requires to be cheap and frequent — rewrite the database file.
//! On the filesystem, evicting an image is one `unlink`.
//!
//! # Why content addressing
//!
//! Capturing the same unchanged window twice is completely routine. Addressing
//! by digest means the second capture costs a row and nothing else, and the
//! retention cap then measures real disk usage rather than a sum of duplicates.

use std::{
    fs::{self, File},
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use scrozz_core::{Error, Result};

use crate::{
    CaptureId,
    hash::is_valid_hash,
    id::is_valid_id,
    record::StoredRecord,
};

/// Directory name under the platform data directory.
pub const APP_DIR: &str = "Scrozz";
/// Filename of the query index.
pub const INDEX_FILE: &str = "index.sqlite";
/// Subdirectory holding content-addressed pixels.
pub const IMAGES_DIR: &str = "images";
/// Subdirectory holding durable per-capture records.
pub const DOCUMENTS_DIR: &str = "documents";

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// What [`StoreLayout::scan_records`] found: the records it could read, and the
/// files it could not, each with the reason it could not be read.
pub type ScannedRecords = (Vec<StoredRecord>, Vec<(PathBuf, String)>);

/// Resolved paths for one history store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLayout {
    root: PathBuf,
}

impl StoreLayout {
    /// A layout rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The default location: `Scrozz` inside the platform data directory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the platform has no data directory, which
    /// happens on a stripped container with no `HOME`. Callers that must work
    /// there pass an explicit root instead.
    pub fn default_location() -> Result<Self> {
        let base = dirs::data_dir().ok_or_else(|| {
            Error::Storage(
                "no platform data directory is available; pass an explicit history path".into(),
            )
        })?;
        Ok(Self::new(base.join(APP_DIR)))
    }

    /// Root of the store.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of the SQLite index.
    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILE)
    }

    /// Directory holding content-addressed pixels.
    #[must_use]
    pub fn images_dir(&self) -> PathBuf {
        self.root.join(IMAGES_DIR)
    }

    /// Directory holding durable records.
    #[must_use]
    pub fn documents_dir(&self) -> PathBuf {
        self.root.join(DOCUMENTS_DIR)
    }

    /// Creates every directory the store needs.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if a directory cannot be created.
    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(self.images_dir())?;
        fs::create_dir_all(self.documents_dir())?;
        Ok(())
    }

    /// Where a blob with this digest lives.
    ///
    /// Two levels of fan-out, 256 ways each. A flat directory of a hundred
    /// thousand files is slow to enumerate on every filesystem we ship on and
    /// pathological on some.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if `hash` is not a digest. The hash comes
    /// from a database column and is about to become a path, so it is validated
    /// rather than trusted.
    pub fn blob_path(&self, hash: &str) -> Result<PathBuf> {
        if !is_valid_hash(hash) {
            return Err(Error::Storage(format!("refusing malformed blob id {hash:?}")));
        }
        Ok(self
            .images_dir()
            .join(&hash[0..2])
            .join(&hash[2..4])
            .join(hash))
    }

    /// Where a capture's durable record lives.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if `id` is not a well-formed identifier.
    pub fn record_path(&self, id: &CaptureId) -> Result<PathBuf> {
        if !is_valid_id(&id.0) {
            return Err(Error::Storage(format!(
                "refusing malformed capture id {:?}",
                id.0
            )));
        }
        Ok(self.documents_dir().join(format!("{}.json", id.0)))
    }

    /// Whether a blob is on disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] for a malformed digest.
    pub fn blob_exists(&self, hash: &str) -> Result<bool> {
        Ok(self.blob_path(hash)?.is_file())
    }

    /// Size of a blob, or `None` if it is not there.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] for a malformed digest.
    pub fn blob_len(&self, hash: &str) -> Result<Option<u64>> {
        match fs::metadata(self.blob_path(hash)?) {
            Ok(meta) => Ok(Some(meta.len())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Writes `data` under its digest, returning whether it was new.
    ///
    /// Idempotent by construction: the name *is* the content, so a blob that is
    /// already there is already correct and re-writing it would only risk
    /// tearing a file another process is reading.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the write fails.
    pub fn write_blob(&self, hash: &str, data: &[u8]) -> Result<bool> {
        let path = self.blob_path(hash)?;
        if path.is_file() {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, data)?;
        Ok(true)
    }

    /// Reads a blob, or `None` if it has been evicted.
    ///
    /// # Errors
    ///
    /// Returns an I/O error for anything other than absence.
    pub fn read_blob(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        match fs::read(self.blob_path(hash)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Removes a blob, returning whether it was there.
    ///
    /// # Errors
    ///
    /// Returns an I/O error for anything other than absence.
    pub fn delete_blob(&self, hash: &str) -> Result<bool> {
        match fs::remove_file(self.blob_path(hash)?) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// Writes a capture's durable record.
    ///
    /// # Errors
    ///
    /// Returns an I/O or encoding error.
    pub fn write_record(&self, record: &StoredRecord) -> Result<()> {
        let path = self.record_path(&CaptureId(record.id.clone()))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, &record.to_json()?)
    }

    /// Reads a capture's durable record, or `None` if there is none.
    ///
    /// # Errors
    ///
    /// Returns an I/O or decoding error.
    pub fn read_record(&self, id: &CaptureId) -> Result<Option<StoredRecord>> {
        match fs::read(self.record_path(id)?) {
            Ok(bytes) => StoredRecord::from_json(&bytes).map(Some),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Removes a capture's durable record, returning whether it was there.
    ///
    /// # Errors
    ///
    /// Returns an I/O error for anything other than absence.
    pub fn delete_record(&self, id: &CaptureId) -> Result<bool> {
        match fs::remove_file(self.record_path(id)?) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// Every readable record on disk, with unreadable ones reported separately.
    ///
    /// Used to rebuild the index. One damaged sidecar must not stop the other
    /// nine thousand from being recovered, so failures are collected rather
    /// than propagated.
    ///
    /// # Errors
    ///
    /// Returns an I/O error only if the documents directory itself cannot be
    /// listed.
    pub fn scan_records(&self) -> Result<ScannedRecords> {
        let mut records = Vec::new();
        let mut failures = Vec::new();

        let dir = self.documents_dir();
        if !dir.is_dir() {
            return Ok((records, failures));
        }

        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            match fs::read(&path).map_err(Error::from).and_then(|bytes| StoredRecord::from_json(&bytes)) {
                Ok(record) if is_valid_id(&record.id) => records.push(record),
                Ok(record) => failures.push((path, format!("malformed capture id {:?}", record.id))),
                Err(err) => failures.push((path, err.to_string())),
            }
        }

        records.sort_by(|a, b| a.id.cmp(&b.id));
        Ok((records, failures))
    }

    /// Every blob on disk, as `(digest, byte length)`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the images tree cannot be walked.
    pub fn scan_blobs(&self) -> Result<Vec<(String, u64)>> {
        let mut blobs = Vec::new();
        let images = self.images_dir();
        if !images.is_dir() {
            return Ok(blobs);
        }

        for outer in fs::read_dir(&images)? {
            let outer = outer?;
            if !outer.file_type()?.is_dir() {
                continue;
            }
            for inner in fs::read_dir(outer.path())? {
                let inner = inner?;
                if !inner.file_type()?.is_dir() {
                    continue;
                }
                for blob in fs::read_dir(inner.path())? {
                    let blob = blob?;
                    let name = blob.file_name().to_string_lossy().into_owned();
                    if !is_valid_hash(&name) {
                        continue;
                    }
                    blobs.push((name, blob.metadata()?.len()));
                }
            }
        }

        blobs.sort();
        Ok(blobs)
    }

    /// Moves the index and its write-ahead files aside, preserving them.
    ///
    /// Returns the path the index was moved to. Deliberately a move rather than
    /// a delete: a corrupt database is still evidence, and `.recover` can often
    /// pull rows out of one by hand later.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the index cannot be moved.
    pub fn quarantine_index(&self, stamp: i64) -> Result<Option<PathBuf>> {
        let index = self.index_path();
        if !index.exists() {
            return Ok(None);
        }
        let quarantined = self.root.join(format!("{INDEX_FILE}.corrupt-{stamp}"));
        fs::rename(&index, &quarantined)?;

        // The -wal and -shm files belong to the quarantined database; leaving
        // them behind would have SQLite try to replay one database's log into
        // another, which turns a recoverable event into a genuinely lost one.
        for suffix in ["-wal", "-shm"] {
            let side = self.root.join(format!("{INDEX_FILE}{suffix}"));
            if side.exists() {
                let _ = fs::rename(
                    &side,
                    self.root
                        .join(format!("{INDEX_FILE}.corrupt-{stamp}{suffix}")),
                );
            }
        }
        Ok(Some(quarantined))
    }
}

/// Writes `bytes` to `path` so that a reader sees either the old file or the
/// whole new one, never a half-written one.
///
/// Write to a sibling temporary, flush it to the platter, then rename. Rename
/// within a directory is atomic on every filesystem Scrozz targets, which is
/// what makes a mid-write power cut leave history intact rather than truncated.
///
/// # Errors
///
/// Returns an I/O error if any step fails. The temporary is removed on failure.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Storage(format!("{} has no parent directory", path.display())))?;

    let temp = parent.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));

    let write = (|| -> Result<()> {
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();

    if write.is_err() {
        let _ = fs::remove_file(&temp);
        return write;
    }

    // Durability of the rename itself needs the directory flushed too. Windows
    // has no directory handle to sync, so this is best-effort by design.
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hash::content_hash, test_support::scratch_dir};

    #[test]
    fn blob_paths_fan_out_two_levels() {
        let layout = StoreLayout::new("/history");
        let hash = "ab".to_owned() + &"c".repeat(62);
        let path = layout.blob_path(&hash).expect("valid hash");
        assert!(path.ends_with(format!("images/ab/cc/{hash}")), "{path:?}");
    }

    #[test]
    fn a_hash_that_is_really_a_path_is_refused() {
        let layout = StoreLayout::new("/history");
        assert!(layout.blob_path("../../../etc/passwd").is_err());
        assert!(layout.blob_path("").is_err());
    }

    #[test]
    fn an_id_that_is_really_a_path_is_refused() {
        let layout = StoreLayout::new("/history");
        assert!(
            layout
                .record_path(&CaptureId("../../secrets".into()))
                .is_err()
        );
    }

    #[test]
    fn blobs_are_written_once_and_read_back() {
        let dir = scratch_dir("layout-blobs");
        let layout = StoreLayout::new(dir.path());
        layout.ensure_dirs().expect("dirs");

        let data = b"some pixels".to_vec();
        let hash = content_hash(&data);

        assert!(layout.write_blob(&hash, &data).expect("write"), "first write is new");
        assert!(
            !layout.write_blob(&hash, &data).expect("write"),
            "identical content is already stored"
        );
        assert_eq!(layout.read_blob(&hash).expect("read"), Some(data.clone()));
        assert_eq!(layout.blob_len(&hash).expect("len"), Some(data.len() as u64));

        assert!(layout.delete_blob(&hash).expect("delete"));
        assert!(!layout.delete_blob(&hash).expect("delete again"));
        assert_eq!(layout.read_blob(&hash).expect("read"), None);
    }

    #[test]
    fn scanning_survives_one_unreadable_sidecar() {
        let dir = scratch_dir("layout-scan");
        let layout = StoreLayout::new(dir.path());
        layout.ensure_dirs().expect("dirs");

        let good = crate::test_support::sample_record("Safari", 1);
        layout.write_record(&good).expect("write good");
        fs::write(layout.documents_dir().join("BROKEN.json"), b"{ not json").expect("write bad");

        let (records, failures) = layout.scan_records().expect("scan");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, good.id);
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn atomic_write_replaces_content_wholesale() {
        let dir = scratch_dir("layout-atomic");
        let path = dir.path().join("file.bin");

        atomic_write(&path, b"first").expect("write");
        assert_eq!(fs::read(&path).expect("read"), b"first");

        atomic_write(&path, b"second, longer").expect("overwrite");
        assert_eq!(fs::read(&path).expect("read"), b"second, longer");

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("list")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files must not survive");
    }
}
