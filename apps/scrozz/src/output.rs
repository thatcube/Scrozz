//! The one path from encoded capture bytes to a user-visible file.
//!
//! Keeping this shared by CLI and GUI is not tidiness; it prevents data loss.
//! Both previously built names from whole-second timestamps and called
//! `std::fs::write`, so a second save in the same second silently truncated the
//! first. [`FileExporter`] already solves the race with `File::create_new` and
//! numbered collision suffixes. Every default save goes through it.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use scrozz_core::Error as CoreError;
use scrozz_export::{
    ClipboardDelivery, Destination, EncodeOptions, Encoder as _, FileExporter, FrameEncoder,
    ImageFormat, NamePolicy, NameTemplate, NamingContext, SystemClipboard,
};

use crate::{
    fault::{CliError, CliResult},
    settings_store::SettingsStore,
};

const CLIPBOARD_STAGE_DIRECTORY: &str = "clipboard-files";
const CLIPBOARD_STAGE_MARKER: &str = ".scrozz-clipboard-entry";
const MAX_CLIPBOARD_STAGE_ENTRIES: usize = 8;
const MAX_CLIPBOARD_STAGE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CLIPBOARD_STAGE_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
static NEXT_CLIPBOARD_STAGE: AtomicU64 = AtomicU64::new(0);

/// Which representations one clipboard copy contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardMode {
    /// Native image flavours only.
    Image,
    /// A file reference only.
    File,
    /// Native image flavours and a file reference in one transaction.
    ImageAndFile,
}

impl ClipboardMode {
    fn parse(value: &str) -> Result<Self, CoreError> {
        match value {
            "image" => Ok(Self::Image),
            "file" => Ok(Self::File),
            "image-and-file" => Ok(Self::ImageAndFile),
            other => Err(CoreError::InvalidRequest(format!(
                "clipboard.mode has unsupported value {other:?}"
            ))),
        }
    }

    const fn includes_image(self) -> bool {
        matches!(self, Self::Image | Self::ImageAndFile)
    }

    const fn includes_file(self) -> bool {
        matches!(self, Self::File | Self::ImageAndFile)
    }
}

/// The capture settings needed before pixels are requested or exported.
#[derive(Debug, Clone)]
pub struct CaptureOutput {
    directory: PathBuf,
    template: NameTemplate,
    format: ImageFormat,
    quality: u8,
    include_cursor: bool,
    copy_to_clipboard: bool,
    clipboard_mode: ClipboardMode,
    clipboard_stage_root: PathBuf,
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
            clipboard_mode: ClipboardMode::parse(store.get("clipboard.mode")?.1)?,
            clipboard_stage_root: store
                .path()
                .parent()
                .ok_or_else(|| {
                    CliError::Core(CoreError::Storage(
                        "settings path does not have a parent directory".to_owned(),
                    ))
                })?
                .join(CLIPBOARD_STAGE_DIRECTORY),
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

    /// Which representations each clipboard copy contains.
    #[must_use]
    pub const fn clipboard_mode(&self) -> ClipboardMode {
        self.clipboard_mode
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

    /// The configured folder and a legal suggested filename.
    ///
    /// The path is not reserved and must not be written without an atomic
    /// collision policy. It is intended for native save dialogs, which perform
    /// their own overwrite confirmation.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured directory leaves no room for a
    /// legal filename.
    pub fn suggested_path(&self, context: &NamingContext) -> CliResult<PathBuf> {
        self.suggested_path_for(context, self.format)
    }

    /// Suggests a path whose extension matches already-encoded bytes.
    ///
    /// This matters for history-rehydrated cards, whose durable rendering is
    /// PNG even when the current default export format has since changed.
    ///
    /// # Errors
    ///
    /// Returns a codec error when the bytes are not a supported image, or the
    /// same path-budget error as [`Self::suggested_path`].
    pub fn suggested_path_for_bytes(
        &self,
        context: &NamingContext,
        bytes: &[u8],
    ) -> CliResult<PathBuf> {
        let format = ImageFormat::sniff(bytes).ok_or_else(|| {
            CliError::Core(CoreError::Codec(
                "capture bytes are not PNG, JPEG, or WebP".to_owned(),
            ))
        })?;
        self.suggested_path_for(context, format)
    }

    fn suggested_path_for(
        &self,
        context: &NamingContext,
        format: ImageFormat,
    ) -> CliResult<PathBuf> {
        let name = NamePolicy::default().file_name(
            &self.template,
            context,
            format.extension(),
            Some(&self.directory),
        )?;
        Ok(self.directory.join(name))
    }

    /// Writes encoded bytes to an exact user-approved path.
    ///
    /// The native save dialog owns overwrite consent. This method creates a
    /// previously missing parent directory, if one was selected, and then
    /// writes the exact path rather than applying the default-folder collision
    /// suffix policy.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the parent cannot be created or the file
    /// cannot be written.
    pub fn export_as(&self, bytes: &[u8], path: &Path) -> CliResult<PathBuf> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
        Ok(path.to_owned())
    }

    /// Offers this capture to the clipboard using the persisted clipboard mode.
    ///
    /// File modes use a bounded staging pool rather than the export folder.
    /// Entries survive process exit for consumers that resolve file URLs
    /// lazily. Cleanup runs only after the new clipboard transaction succeeds,
    /// so the previous clipboard file is not removed before its replacement.
    ///
    /// # Errors
    ///
    /// Returns an error when encoding or staging fails, or when the native
    /// clipboard cannot commit and verify every requested representation.
    pub fn write_clipboard(
        &self,
        frame: &scrozz_core::Frame,
        context: &NamingContext,
    ) -> CliResult<ClipboardDelivery> {
        let bytes = self.encoder(None).encode(frame, self.format)?;
        self.write_clipboard_encoded(frame, &bytes, self.format, context)
    }

    /// Offers a capture to the clipboard without re-encoding file-mode bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when `bytes` do not match `format`, staging fails, or
    /// the native clipboard omits a requested representation.
    pub fn write_clipboard_encoded(
        &self,
        frame: &scrozz_core::Frame,
        bytes: &[u8],
        format: ImageFormat,
        context: &NamingContext,
    ) -> CliResult<ClipboardDelivery> {
        let staged = if self.clipboard_mode.includes_file() {
            if ImageFormat::sniff(bytes) != Some(format) {
                return Err(CliError::Core(CoreError::InvalidRequest(format!(
                    "clipboard file bytes do not match the requested {} format",
                    format.extension()
                ))));
            }
            Some(self.stage_clipboard_file(bytes, format, context)?)
        } else {
            None
        };
        let files = staged
            .as_ref()
            .map(|entry| vec![entry.path.clone()])
            .unwrap_or_default();

        let result = SystemClipboard::new().write_content(
            self.clipboard_mode.includes_image().then_some(frame),
            &files,
        );
        match result {
            Ok(delivery) => {
                if let Some(entry) = staged.as_ref()
                    && let Err(error) = self.prune_clipboard_stage(&entry.directory)
                {
                    tracing::warn!(
                        path = %self.clipboard_stage_root.display(),
                        "clipboard file pool cleanup failed: {error}"
                    );
                }
                Ok(delivery)
            }
            Err(error) => {
                if let Some(entry) = staged
                    && let Err(cleanup) = fs::remove_dir_all(&entry.directory)
                {
                    tracing::warn!(
                        path = %entry.directory.display(),
                        "failed to remove an uncommitted clipboard staging entry: {cleanup}"
                    );
                }
                Err(error.into())
            }
        }
    }

    fn stage_clipboard_file(
        &self,
        bytes: &[u8],
        format: ImageFormat,
        context: &NamingContext,
    ) -> CliResult<StagedClipboardFile> {
        fs::create_dir_all(&self.clipboard_stage_root)?;
        let directory = loop {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let nonce = NEXT_CLIPBOARD_STAGE.fetch_add(1, Ordering::Relaxed);
            let candidate = self.clipboard_stage_root.join(format!(
                "{stamp:032x}-{:08x}-{nonce:016x}",
                std::process::id()
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        };

        let result = (|| -> CliResult<PathBuf> {
            fs::write(directory.join(CLIPBOARD_STAGE_MARKER), b"scrozz\n")?;
            let suggested = self.suggested_path_for(context, format)?;
            let name = suggested.file_name().ok_or_else(|| {
                CliError::Core(CoreError::Storage(
                    "capture filename template did not produce a filename".to_owned(),
                ))
            })?;
            let path = directory.join(name);
            fs::write(&path, bytes)?;
            Ok(path)
        })();
        match result {
            Ok(path) => Ok(StagedClipboardFile { directory, path }),
            Err(error) => {
                if let Err(cleanup) = fs::remove_dir_all(&directory) {
                    tracing::warn!(
                        path = %directory.display(),
                        "failed to remove an incomplete clipboard staging entry: {cleanup}"
                    );
                }
                Err(error)
            }
        }
    }

    fn prune_clipboard_stage(&self, current: &Path) -> CliResult<()> {
        let now = SystemTime::now();
        let mut entries = fs::read_dir(&self.clipboard_stage_root)?
            .filter_map(|item| match item {
                Ok(item) => match owned_clipboard_stage_directory(&item.path()) {
                    Ok(true) => Some(item.path()),
                    Ok(false) => None,
                    Err(error) => {
                        tracing::warn!(
                            path = %item.path().display(),
                            "could not verify clipboard staging entry ownership: {error}"
                        );
                        None
                    }
                },
                Err(error) => {
                    tracing::warn!("could not inspect clipboard staging entry: {error}");
                    None
                }
            })
            .filter_map(|directory| match clipboard_stage_metadata(&directory) {
                Ok(metadata) => Some((directory, metadata)),
                Err(error) => {
                    tracing::warn!(
                        path = %directory.display(),
                        "could not inspect clipboard staging entry: {error}"
                    );
                    None
                }
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|(directory, metadata)| {
            (directory != current, std::cmp::Reverse(metadata.modified))
        });

        let mut kept = 0usize;
        let mut kept_bytes = 0u64;
        for (directory, metadata) in entries {
            let current_entry = directory == current;
            let expired = now
                .duration_since(metadata.modified)
                .is_ok_and(|age| age > MAX_CLIPBOARD_STAGE_AGE);
            let exceeds_count = kept >= MAX_CLIPBOARD_STAGE_ENTRIES;
            let exceeds_bytes =
                kept_bytes.saturating_add(metadata.bytes) > MAX_CLIPBOARD_STAGE_BYTES;
            if !current_entry && (expired || exceeds_count || exceeds_bytes) {
                fs::remove_dir_all(directory)?;
            } else {
                kept = kept.saturating_add(1);
                kept_bytes = kept_bytes.saturating_add(metadata.bytes);
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct StagedClipboardFile {
    directory: PathBuf,
    path: PathBuf,
}

#[derive(Debug)]
struct ClipboardStageMetadata {
    modified: SystemTime,
    bytes: u64,
}

fn owned_clipboard_stage_directory(directory: &Path) -> std::io::Result<bool> {
    let Some(name) = directory.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    let mut segments = name.split('-');
    let valid_name = matches!(
        (segments.next(), segments.next(), segments.next(), segments.next()),
        (Some(stamp), Some(pid), Some(nonce), None)
            if is_lower_hex(stamp, 32) && is_lower_hex(pid, 8) && is_lower_hex(nonce, 16)
    );
    if !valid_name {
        return Ok(false);
    }

    let directory_metadata = fs::symlink_metadata(directory)?;
    if !directory_metadata.file_type().is_dir() {
        return Ok(false);
    }
    let marker = directory.join(CLIPBOARD_STAGE_MARKER);
    let marker_metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !marker_metadata.file_type().is_file() {
        return Ok(false);
    }
    Ok(fs::read(marker)? == b"scrozz\n")
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn clipboard_stage_metadata(directory: &Path) -> std::io::Result<ClipboardStageMetadata> {
    let mut modified = UNIX_EPOCH;
    let mut bytes = 0u64;
    let mut payloads = 0usize;
    for item in fs::read_dir(directory)? {
        let item = item?;
        if item.file_name() == CLIPBOARD_STAGE_MARKER {
            continue;
        }
        let metadata = fs::symlink_metadata(item.path())?;
        if !metadata.file_type().is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "clipboard staging entries may contain only regular files",
            ));
        }
        payloads = payloads.saturating_add(1);
        bytes = bytes.saturating_add(metadata.len());
        modified = modified.max(metadata.modified()?);
    }
    if payloads != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "clipboard staging entry must contain exactly one payload",
        ));
    }
    Ok(ClipboardStageMetadata { modified, bytes })
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
        store.set("clipboard.mode", "image-and-file").unwrap();

        let output = CaptureOutput::from_store(&store).unwrap();

        assert_eq!(output.format(), ImageFormat::Jpeg);
        assert_eq!(output.clipboard_mode(), ClipboardMode::ImageAndFile);
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

    #[test]
    fn clipboard_file_pool_is_bounded_and_ignores_foreign_directories() {
        let directory = scratch();
        let mut store = SettingsStore::open(directory.join("settings.json")).unwrap();
        store
            .set(
                "capture.folder",
                directory.join("captures").to_str().unwrap(),
            )
            .unwrap();
        store.set("clipboard.mode", "file").unwrap();
        let output = CaptureOutput::from_store(&store).unwrap();
        let foreign = output.clipboard_stage_root.join("not-owned-by-scrozz");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("keep.txt"), b"keep").unwrap();
        let malformed = output.clipboard_stage_root.join(format!(
            "{:032x}-{:08x}-{:016x}",
            1,
            std::process::id(),
            1
        ));
        fs::create_dir_all(&malformed).unwrap();
        fs::write(malformed.join(CLIPBOARD_STAGE_MARKER), b"not our marker\n").unwrap();
        fs::write(malformed.join("keep.png"), b"keep").unwrap();
        #[cfg(unix)]
        let symlink_target = {
            let target = directory.join("external-entry");
            fs::create_dir_all(&target).unwrap();
            fs::write(target.join(CLIPBOARD_STAGE_MARKER), b"scrozz\n").unwrap();
            fs::write(target.join("keep.png"), b"keep").unwrap();
            std::os::unix::fs::symlink(
                &target,
                output.clipboard_stage_root.join(format!(
                    "{:032x}-{:08x}-{:016x}",
                    2,
                    std::process::id(),
                    2
                )),
            )
            .unwrap();
            target
        };

        let mut current = None;
        for seq in 0..12 {
            current = Some(
                output
                    .stage_clipboard_file(
                        b"\x89PNG\r\n\x1a\nstaged",
                        ImageFormat::Png,
                        &NamingContext {
                            sequence: seq,
                            ..NamingContext::now()
                        },
                    )
                    .unwrap(),
            );
        }
        let current = current.unwrap();
        output.prune_clipboard_stage(&current.directory).unwrap();

        let recognized = fs::read_dir(&output.clipboard_stage_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|item| owned_clipboard_stage_directory(&item.path()).unwrap_or(false))
            .count();
        assert_eq!(recognized, MAX_CLIPBOARD_STAGE_ENTRIES);
        assert!(current.path.is_file());
        assert!(foreign.join("keep.txt").is_file());
        assert!(malformed.join("keep.png").is_file());
        #[cfg(unix)]
        assert!(symlink_target.join("keep.png").is_file());

        fs::remove_dir_all(directory).expect("clean scratch directory");
    }

    #[test]
    fn exact_export_writes_only_the_approved_path() {
        let directory = scratch();
        let store = SettingsStore::open(directory.join("settings.json")).unwrap();
        let output = CaptureOutput::from_store(&store).unwrap();
        let approved = directory.join("chosen").join("Custom.png");

        assert_eq!(
            output.export_as(b"approved bytes", &approved).unwrap(),
            approved
        );
        assert_eq!(fs::read(&approved).unwrap(), b"approved bytes");

        fs::remove_dir_all(directory).expect("clean scratch directory");
    }
}
