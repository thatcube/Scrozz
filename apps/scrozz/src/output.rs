//! The one path from encoded capture bytes to a user-visible file.
//!
//! Keeping this shared by CLI and GUI is not tidiness; it prevents data loss.
//! Both previously built names from whole-second timestamps and called
//! `std::fs::write`, so a second save in the same second silently truncated the
//! first. [`FileExporter`] already solves the race with `File::create_new` and
//! numbered collision suffixes. Every default save goes through it.

use std::path::{Path, PathBuf};

use scrozz_core::Error as CoreError;
use scrozz_export::{
    Destination, EncodeOptions, FileExporter, FrameEncoder, ImageFormat, NameTemplate,
    NamingContext,
};

use crate::{
    fault::{CliError, CliResult},
    settings_store::SettingsStore,
};

/// The capture settings needed before pixels are requested or exported.
#[derive(Debug, Clone)]
pub struct CaptureOutput {
    directory: PathBuf,
    template: NameTemplate,
    format: ImageFormat,
    quality: u8,
    include_cursor: bool,
    copy_to_clipboard: bool,
    include_window_shadow: bool,
}

impl CaptureOutput {
    /// Resolves the current persisted capture settings.
    ///
    /// # Errors
    ///
    /// Returns a settings or storage error when the document cannot be loaded or
    /// contains a value that cannot be represented by the capture pipeline.
    pub fn load() -> CliResult<Self> {
        let store = SettingsStore::load()?;
        Self::from_store(&store)
    }

    /// Resolves capture settings from an already-loaded document.
    ///
    /// # Errors
    ///
    /// Returns an error when a resolved value cannot be represented by the
    /// capture pipeline.
    pub fn from_store(store: &SettingsStore) -> CliResult<Self> {
        let folder = store.get("capture.folder")?.1;
        let format = match store.get("capture.format")?.1 {
            "png" => ImageFormat::Png,
            "jpeg" => ImageFormat::Jpeg,
            "webp" => ImageFormat::WebP,
            value => return Err(invalid_persisted("capture.format", value)),
        };
        let quality_value = store.get("capture.quality")?.1;
        let quality = quality_value
            .parse::<u8>()
            .map_err(|_| invalid_persisted("capture.quality", quality_value))?;
        let template = NameTemplate::parse(store.get("capture.filename-template")?.1)?;

        Ok(Self {
            directory: expand_home(folder)?,
            template,
            format,
            quality,
            include_cursor: resolved_bool(store, "capture.cursor")?,
            copy_to_clipboard: resolved_bool(store, "capture.copy-to-clipboard")?,
            include_window_shadow: resolved_bool(store, "capture.window-shadow")?,
        })
    }

    /// The configured image format.
    #[must_use]
    pub const fn format(&self) -> ImageFormat {
        self.format
    }

    /// The configured lossy encoder quality.
    #[must_use]
    pub const fn quality(&self) -> u8 {
        self.quality
    }

    /// Whether captures include the pointer by default.
    #[must_use]
    pub const fn include_cursor(&self) -> bool {
        self.include_cursor
    }

    /// Whether each capture is also copied to the clipboard.
    #[must_use]
    pub const fn copy_to_clipboard(&self) -> bool {
        self.copy_to_clipboard
    }

    /// Whether window captures include their shadow by default.
    #[must_use]
    pub const fn include_window_shadow(&self) -> bool {
        self.include_window_shadow
    }

    /// Creates an encoder with the configured or explicitly overridden quality.
    #[must_use]
    pub fn encoder(&self, quality: Option<u8>) -> FrameEncoder {
        let options = EncodeOptions {
            jpeg_quality: quality.unwrap_or(self.quality),
            ..EncodeOptions::default()
        };
        FrameEncoder::with_options(options)
    }

    /// Saves encoded image bytes using the configured folder and filename.
    ///
    /// # Errors
    ///
    /// Returns a codec error when `bytes` are not PNG, JPEG, or WebP, and a
    /// storage or I/O error when no safe path can be created.
    pub fn export(&self, bytes: &[u8], context: &NamingContext) -> CliResult<PathBuf> {
        export_to_directory(bytes, &self.directory, context, &self.template)
    }
}

fn resolved_bool(store: &SettingsStore, key: &str) -> CliResult<bool> {
    match store.get(key)?.1 {
        "true" => Ok(true),
        "false" => Ok(false),
        value => Err(invalid_persisted(key, value)),
    }
}

fn invalid_persisted(key: &str, value: &str) -> CliError {
    CliError::Core(CoreError::Storage(format!(
        "persisted setting {key} has unreadable value {value:?}"
    )))
}

fn expand_home(folder: &str) -> CliResult<PathBuf> {
    if folder == "~" {
        return dirs::home_dir().ok_or_else(|| {
            CliError::Core(CoreError::Storage(
                "capture.folder uses `~`, but the home directory is unavailable".to_owned(),
            ))
        });
    }
    if let Some(relative) = folder
        .strip_prefix("~/")
        .or_else(|| folder.strip_prefix("~\\"))
    {
        return dirs::home_dir()
            .map(|home| home.join(relative))
            .ok_or_else(|| {
                CliError::Core(CoreError::Storage(
                    "capture.folder uses `~`, but the home directory is unavailable".to_owned(),
                ))
            });
    }
    Ok(PathBuf::from(folder))
}

fn export_to_directory(
    bytes: &[u8],
    directory: &Path,
    context: &NamingContext,
    template: &NameTemplate,
) -> CliResult<PathBuf> {
    let outcome = FileExporter::new()
        .with_template(template.clone())
        .export_bytes(bytes, &Destination::Folder(directory.to_owned()), context)?;
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
        let template = NameTemplate::default();

        let first =
            export_to_directory(first_bytes, &directory, &context, &template).expect("first save");
        let second = export_to_directory(second_bytes, &directory, &context, &template)
            .expect("second save");

        assert_ne!(first, second, "colliding saves need distinct paths");
        assert_eq!(std::fs::read(first).unwrap(), first_bytes);
        assert_eq!(std::fs::read(second).unwrap(), second_bytes);

        std::fs::remove_dir_all(directory).expect("clean scratch directory");
    }

    #[test]
    fn persisted_capture_output_controls_format_quality_folder_and_template() {
        let directory = scratch();
        let mut store = SettingsStore::open(directory.join("settings.json")).unwrap();
        store
            .set(
                "capture.folder",
                directory.join("captures").to_str().unwrap(),
            )
            .unwrap();
        store.set("capture.format", "jpeg").unwrap();
        store.set("capture.quality", "72").unwrap();
        store
            .set("capture.filename-template", "Shot {width}x{height}")
            .unwrap();

        let output = CaptureOutput::from_store(&store).unwrap();

        assert_eq!(output.format(), ImageFormat::Jpeg);
        assert_eq!(output.encoder(None).options().jpeg_quality, 72);
        assert_eq!(
            output.encoder(Some(81)).options().jpeg_quality,
            81,
            "an explicit CLI quality should win"
        );
        let path = output
            .export(
                b"\x89PNG\r\n\x1a\ncapture",
                &NamingContext {
                    width: 640,
                    height: 480,
                    ..NamingContext::now()
                },
            )
            .unwrap();
        assert_eq!(path.file_name().unwrap(), "Shot 640x480.png");

        std::fs::remove_dir_all(directory).expect("clean scratch directory");
    }
}
