//! Durable state for "retake last region".

use std::{
    fs::{self, File, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use scrozz_core::{Display, DisplayId, Error, LogicalRect, Result, ScaleFactor};
use serde::{Deserialize, Serialize};

const VERSION: u32 = 2;
const APP_DIR: &str = "Scrozz";
const FILE_NAME: &str = "last-region.json";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRegion {
    version: u32,
    rect: LogicalRect,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display: Option<StoredDisplay>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum StoredDisplay {
    Fingerprint(DisplayFingerprint),
    Legacy(DisplayId),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct DisplayFingerprint {
    id: DisplayId,
    name: String,
    bounds: LogicalRect,
    scale: ScaleFactor,
}

impl DisplayFingerprint {
    fn new(display: &Display) -> Self {
        Self {
            id: display.id.clone(),
            name: display.name.clone(),
            bounds: display.bounds,
            scale: display.scale,
        }
    }

    fn matches(&self, display: &Display) -> bool {
        self.id == display.id
            && self.name == display.name
            && self.bounds == display.bounds
            && self.scale == display.scale
    }
}

/// Geometry and owning display retained for a future retake.
#[derive(Debug, Clone, PartialEq)]
pub struct RememberedRegion {
    pub rect: LogicalRect,
    display: Option<DisplayFingerprint>,
}

impl RememberedRegion {
    #[must_use]
    pub fn new(rect: LogicalRect, display: Option<&Display>) -> Self {
        Self {
            rect,
            display: display.map(DisplayFingerprint::new),
        }
    }

    /// Resolves the saved owner only when its current topology still matches.
    #[must_use]
    pub fn display_for(&self, displays: &[Display]) -> Option<DisplayId> {
        let fingerprint = self.display.as_ref()?;
        displays
            .iter()
            .find(|display| fingerprint.matches(display))
            .map(|display| display.id.clone())
    }
}

/// A remembered-region file at a resolved path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RememberedRegionStore {
    path: PathBuf,
}

impl RememberedRegionStore {
    /// Uses an explicit path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Resolves the platform-local state path.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] when this process has no user data directory.
    pub fn default_location() -> Result<Self> {
        let base = dirs::data_local_dir()
            .or_else(dirs::data_dir)
            .ok_or_else(|| {
                Error::Storage(
                    "no platform data directory is available for the remembered region".to_owned(),
                )
            })?;
        Ok(Self::new(base.join(APP_DIR).join(FILE_NAME)))
    }

    /// The file this store reads and replaces.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the last region, or `None` before the first region capture.
    ///
    /// # Errors
    ///
    /// Returns an I/O or storage error for unreadable, malformed, unsupported, or
    /// non-finite state. Corrupt state is never silently interpreted as geometry.
    pub fn load(&self) -> Result<Option<RememberedRegion>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let stored: StoredRegion = serde_json::from_slice(&bytes).map_err(|error| {
            Error::Storage(format!(
                "could not decode remembered region {}: {error}",
                self.path.display()
            ))
        })?;
        if !matches!(stored.version, 1 | VERSION) {
            return Err(Error::Storage(format!(
                "remembered region {} uses unsupported version {}",
                self.path.display(),
                stored.version
            )));
        }
        validate(stored.rect)?;
        let display = stored.display.and_then(|display| match display {
            StoredDisplay::Fingerprint(display) => Some(display),
            StoredDisplay::Legacy(_) => None,
        });
        Ok(Some(RememberedRegion {
            rect: stored.rect,
            display,
        }))
    }

    /// Atomically replaces the remembered region.
    ///
    /// # Errors
    ///
    /// Returns an I/O or storage error if the rectangle is invalid or the file
    /// cannot be written.
    pub fn save(&self, region: RememberedRegion) -> Result<()> {
        validate(region.rect)?;
        let bytes = serde_json::to_vec_pretty(&StoredRegion {
            version: VERSION,
            rect: region.rect,
            display: region.display.map(StoredDisplay::Fingerprint),
        })
        .map_err(|error| Error::Storage(format!("could not encode remembered region: {error}")))?;

        let parent = self.path.parent().ok_or_else(|| {
            Error::Storage(format!(
                "remembered-region path {} has no parent directory",
                self.path.display()
            ))
        })?;
        fs::create_dir_all(parent)?;

        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".{FILE_NAME}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let result = write_and_replace(&temp, &self.path, &bytes);
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

fn validate(rect: LogicalRect) -> Result<()> {
    let values = [
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    ];
    if values.into_iter().any(|value| !value.is_finite())
        || rect.size.width <= 0.0
        || rect.size.height <= 0.0
    {
        return Err(Error::Storage(format!(
            "remembered region must be finite and non-empty, got {rect:?}"
        )));
    }
    Ok(())
}

fn write_and_replace(temp: &Path, destination: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = create_new(temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, destination)?;
    Ok(())
}

fn create_new(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().write(true).create_new(true).open(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{LogicalPoint, LogicalSize};

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "scrozz-remembered-region-{}-{}-{name}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> LogicalRect {
        LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(width, height))
    }

    fn display(id: &str, bounds: LogicalRect, scale: f64) -> Display {
        Display {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds,
            work_area: bounds,
            scale: ScaleFactor::new(scale),
            is_primary: true,
        }
    }

    #[test]
    fn absence_is_distinct_from_a_storage_failure() {
        let root = scratch("absent");
        let store = RememberedRegionStore::new(root.join(FILE_NAME));
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn a_region_round_trips_with_negative_desktop_coordinates() {
        let root = scratch("round-trip");
        let store = RememberedRegionStore::new(root.join(FILE_NAME));
        let expected = rect(-1440.0, 12.5, 800.25, 600.5);

        let owner = display("external-left", rect(-1440.0, 0.0, 1440.0, 900.0), 1.5);
        let expected = RememberedRegion::new(expected, Some(&owner));
        store.save(expected.clone()).unwrap();

        assert_eq!(store.load().unwrap(), Some(expected));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn replacement_never_appends_a_second_document() {
        let root = scratch("replace");
        let store = RememberedRegionStore::new(root.join(FILE_NAME));
        store
            .save(RememberedRegion::new(rect(0.0, 0.0, 100.0, 100.0), None))
            .unwrap();
        let expected = rect(50.0, 60.0, 300.0, 200.0);

        store.save(RememberedRegion::new(expected, None)).unwrap();

        assert_eq!(
            store.load().unwrap(),
            Some(RememberedRegion::new(expected, None))
        );
        let text = fs::read_to_string(store.path()).unwrap();
        assert_eq!(text.matches("\"version\"").count(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_and_non_finite_state_is_refused() {
        let root = scratch("invalid");
        let store = RememberedRegionStore::new(root.join(FILE_NAME));
        fs::create_dir_all(&root).unwrap();
        fs::write(store.path(), b"{not json").unwrap();
        assert!(matches!(store.load(), Err(Error::Storage(_))));

        fs::write(
            store.path(),
            br#"{"version":1,"rect":{"origin":{"x":0.0,"y":0.0},"size":{"width":0.0,"height":2.0}}}"#,
        )
        .unwrap();
        assert!(matches!(store.load(), Err(Error::Storage(_))));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unknown_versions_are_not_guessed() {
        let root = scratch("version");
        let store = RememberedRegionStore::new(root.join(FILE_NAME));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            store.path(),
            br#"{"version":99,"rect":{"origin":{"x":0.0,"y":0.0},"size":{"width":1.0,"height":2.0}}}"#,
        )
        .unwrap();

        assert!(matches!(store.load(), Err(Error::Storage(_))));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_files_without_a_display_remain_readable() {
        let root = scratch("legacy");
        let store = RememberedRegionStore::new(root.join(FILE_NAME));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            store.path(),
            br#"{"version":1,"rect":{"origin":{"x":12.0,"y":24.0},"size":{"width":80.0,"height":60.0}}}"#,
        )
        .unwrap();

        assert_eq!(
            store.load().unwrap(),
            Some(RememberedRegion::new(rect(12.0, 24.0, 80.0, 60.0), None))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_saved_display_id_is_only_reused_with_matching_topology() {
        let root = scratch("display-fingerprint");
        let store = RememberedRegionStore::new(root.join(FILE_NAME));
        let bounds = rect(100.0, 0.0, 200.0, 120.0);
        let owner = display("external", bounds, 1.5);
        store
            .save(RememberedRegion::new(
                rect(120.0, 20.0, 80.0, 60.0),
                Some(&owner),
            ))
            .unwrap();
        let remembered = store.load().unwrap().unwrap();

        assert_eq!(
            remembered.display_for(std::slice::from_ref(&owner)),
            Some(owner.id.clone())
        );
        let moved = display("external", rect(0.0, 0.0, 200.0, 120.0), 1.5);
        assert_eq!(remembered.display_for(&[moved]), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_session_only_display_ids_are_not_trusted_after_reload() {
        let root = scratch("legacy-display");
        let store = RememberedRegionStore::new(root.join(FILE_NAME));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            store.path(),
            br#"{"version":1,"rect":{"origin":{"x":12.0,"y":24.0},"size":{"width":80.0,"height":60.0}},"display":"external"}"#,
        )
        .unwrap();
        let remembered = store.load().unwrap().unwrap();
        let current = display("external", rect(0.0, 0.0, 200.0, 120.0), 1.0);

        assert_eq!(remembered.display_for(&[current]), None);
        let _ = fs::remove_dir_all(root);
    }
}
