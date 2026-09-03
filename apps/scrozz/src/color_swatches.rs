//! Persistent custom annotation colours.

use std::path::{Path, PathBuf};

use scrozz_annotate::Color;
use scrozz_core::{Error, Result};
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;
const FILE_NAME: &str = "colors.json";

#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    version: u32,
    colors: Vec<Color>,
}

/// Disk location for the editor's MRU custom palette.
#[derive(Debug, Clone)]
pub struct CustomSwatchStore {
    path: PathBuf,
}

impl CustomSwatchStore {
    /// Uses the same per-user Scrozz configuration directory as shortcuts.
    pub fn default_location() -> Result<Self> {
        let root = dirs::config_dir().ok_or_else(|| {
            Error::Platform("the operating system has no user configuration directory".to_owned())
        })?;
        Ok(Self {
            path: root.join("Scrozz").join(FILE_NAME),
        })
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Loads and sanitises at most eight MRU colours.
    pub fn load(&self) -> Result<Vec<Color>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Error::Io(error)),
        };
        let stored: Stored = serde_json::from_slice(&bytes).map_err(|error| {
            Error::InvalidRequest(format!("custom colours are invalid: {error}"))
        })?;
        if stored.version > VERSION {
            return Err(Error::InvalidRequest(format!(
                "custom colour version {} is newer than supported version {VERSION}",
                stored.version
            )));
        }
        Ok(sanitise(stored.colors))
    }

    /// Atomically stores the sanitised MRU palette.
    pub fn save(&self, colors: &[Color]) -> Result<()> {
        let Some(parent) = self.path.parent() else {
            return Err(Error::InvalidRequest(
                "custom colour path has no parent".to_owned(),
            ));
        };
        std::fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec_pretty(&Stored {
            version: VERSION,
            colors: sanitise(colors.to_vec()),
        })
        .map_err(|error| {
            Error::Platform(format!("custom colours could not be encoded: {error}"))
        })?;
        let temporary = temporary_path(&self.path);
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(&temporary, &self.path)?;
        Ok(())
    }
}

fn sanitise(colors: Vec<Color>) -> Vec<Color> {
    let mut out = Vec::new();
    for color in colors {
        if !out.contains(&color) {
            out.push(color);
        }
        if out.len() == scrozz_ui::editor::toolbar::MAX_CUSTOM_SWATCHES {
            break;
        }
    }
    out
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn palette_round_trip_preserves_alpha_order_and_bound() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "scrozz-custom-colours-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let store = CustomSwatchStore::at(root.join(FILE_NAME));
        let colors: Vec<_> = (0..12)
            .map(|value| Color::rgba(value, value + 1, value + 2, 128))
            .collect();
        store.save(&colors).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 8);
        assert_eq!(loaded[0], colors[0]);
        assert_eq!(loaded[7], colors[7]);
        let _ = std::fs::remove_dir_all(root);
    }
}
