//! Durable state for the one-time OCR introduction.

use std::{
    fs,
    io::{Error, ErrorKind},
    path::PathBuf,
};

const MARKER_FILE: &str = "ocr-onboarding-v1.seen";

/// Whether the GUI can currently deliver the workflow described by onboarding.
///
/// Keep this false until region selection runs OCR, copies its text, and exposes
/// real persisted OCR settings. The CLI backend alone is not the promised GUI
/// capability.
#[must_use]
pub const fn workflow_available() -> bool {
    false
}

/// The durable marker controlling whether OCR onboarding appears automatically.
#[derive(Debug, Clone)]
pub struct OcrOnboardingMemory {
    marker: Option<PathBuf>,
}

impl OcrOnboardingMemory {
    /// Uses the platform's local application-data directory.
    #[must_use]
    pub fn system() -> Self {
        Self {
            marker: dirs::data_local_dir()
                .map(|base| base.join("scrozz").join("onboarding").join(MARKER_FILE)),
        }
    }

    /// Whether this installation has completed or skipped the introduction.
    #[must_use]
    pub fn has_seen(&self) -> bool {
        self.marker.as_ref().is_some_and(|path| path.is_file())
    }

    /// Records completion without requesting any platform permission.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the application-data directory is unavailable
    /// or the marker cannot be written.
    pub fn mark_seen(&self) -> Result<(), Error> {
        let marker = self.marker.as_ref().ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                "the platform did not provide a local application-data directory",
            )
        })?;
        let parent = marker.parent().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "the OCR onboarding marker has no parent directory",
            )
        })?;
        fs::create_dir_all(parent)?;
        fs::write(marker, b"seen\n")
    }

    #[cfg(test)]
    fn at(marker: PathBuf) -> Self {
        Self {
            marker: Some(marker),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn marker_is_durable_and_contains_no_user_data() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "scrozz-ocr-onboarding-{}-{unique}",
            std::process::id()
        ));
        let marker = directory.join(MARKER_FILE);
        let memory = OcrOnboardingMemory::at(marker.clone());

        assert!(!memory.has_seen());
        memory.mark_seen().unwrap();
        assert!(memory.has_seen());
        assert_eq!(fs::read(&marker).unwrap(), b"seen\n");

        fs::remove_file(marker).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn onboarding_stays_gated_until_the_gui_workflow_exists() {
        assert!(!workflow_available());
    }
}
