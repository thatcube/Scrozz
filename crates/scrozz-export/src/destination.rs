//! Delivering encoded captures to where the user asked for them.
//!
//! Decision D18 splits this in two, and the split is the whole design:
//!
//! - [`Destination::Folder`] is *any* folder. That is not a small feature — it
//!   is what makes a Dropbox, iCloud or Syncthing directory deliver sync with
//!   no service on our side, no account, and no bill to anybody.
//! - [`Destination::S3`] exists for the one thing a folder cannot do: produce a
//!   URL at the moment of capture. Bring-your-own bucket means shareable links
//!   cost the project nothing, cannot be switched off, and leave the user
//!   owning their data.
//!
//! D18 also fixes a hard constraint that this module cannot enforce alone but
//! is written to permit: **export happens off the capture path.** Everything
//! here is synchronous and blocking, and is meant to be called from a queue, not
//! from the code that took the screenshot. Writing to a slow SMB share on the
//! capture thread would stall the app at the worst possible moment.

use std::{
    fmt,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use scrozz_core::{ColorSpace, Error, Frame, PhysicalSize, PixelFormat, Result, ScaleFactor};

use crate::{
    Clipboard, Destination, Encoder, Exporter, ImageFormat,
    clipboard::{ClipboardReport, SystemClipboard},
    encode::FrameEncoder,
    naming::{NamePolicy, NameTemplate, NamingContext},
};

// ---------------------------------------------------------------------------
// S3
// ---------------------------------------------------------------------------

/// One object to be PUT into a bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Object<'a> {
    /// Bucket name.
    pub bucket: &'a str,
    /// Full key, prefix included.
    pub key: &'a str,
    /// The encoded capture.
    pub bytes: &'a [u8],
    /// The media type to store the object with, so a browser renders the link
    /// rather than downloading it.
    pub content_type: &'a str,
}

/// Uploads objects to an S3-compatible bucket.
///
/// Deliberately a trait with no implementation in this crate. Every S3-alike —
/// AWS, Cloudflare R2, Backblaze B2, MinIO — speaks the same handful of
/// requests, and a full SDK is a very large dependency to carry for a single
/// authenticated PUT. Keeping the seam here means `scrozz-cloud` is a swappable
/// implementation, a test can substitute a fake, and this crate stays buildable
/// and testable with no network stack at all.
pub trait S3Uploader: fmt::Debug + Send + Sync {
    /// Uploads an object and returns the URL it can be fetched from.
    ///
    /// # Errors
    ///
    /// Returns an error if the upload failed. Implementations should treat this
    /// as retryable where the cause is transient: per D18 uploads are queued
    /// with visible progress and retry, so a failure here is a queue state, not
    /// a lost capture.
    fn upload(&self, object: &S3Object<'_>) -> Result<String>;
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// What an export produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExportOutcome {
    /// Where the file landed, for a folder export.
    pub path: Option<PathBuf>,
    /// A URL the capture can be fetched from.
    pub url: Option<String>,
    /// Which clipboard flavours were delivered, for a clipboard export.
    pub clipboard: Option<ClipboardReport>,
}

// ---------------------------------------------------------------------------
// The exporter
// ---------------------------------------------------------------------------

/// Writes captures to folders, the clipboard, and S3-compatible storage.
pub struct FileExporter {
    /// The filename pattern.
    pub template: NameTemplate,
    /// How that pattern becomes a legal filename.
    pub policy: NamePolicy,
    encoder: FrameEncoder,
    clipboard: Box<dyn Clipboard + Send + Sync>,
    uploader: Option<Box<dyn S3Uploader>>,
    /// The public base a bucket is served from, e.g. `https://cdn.example.com`.
    base_url: Option<String>,
}

impl fmt::Debug for FileExporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileExporter")
            .field("template", &self.template)
            .field("policy", &self.policy)
            .field("uploader", &self.uploader)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl Default for FileExporter {
    fn default() -> Self {
        Self {
            template: NameTemplate::default(),
            policy: NamePolicy::default(),
            encoder: FrameEncoder::new(),
            clipboard: Box::new(SystemClipboard::new()),
            uploader: None,
            base_url: None,
        }
    }
}

impl FileExporter {
    /// An exporter with the default template and policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the filename pattern.
    #[must_use]
    pub fn with_template(mut self, template: NameTemplate) -> Self {
        self.template = template;
        self
    }

    /// Sets the naming policy.
    #[must_use]
    pub fn with_policy(mut self, policy: NamePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Sets the encoder.
    #[must_use]
    pub fn with_encoder(mut self, encoder: FrameEncoder) -> Self {
        self.encoder = encoder;
        self
    }

    /// Substitutes the clipboard, for tests and for a future native backend.
    #[must_use]
    pub fn with_clipboard(mut self, clipboard: Box<dyn Clipboard + Send + Sync>) -> Self {
        self.clipboard = clipboard;
        self
    }

    /// Installs an uploader and the base URL its bucket is served from.
    #[must_use]
    pub fn with_uploader(
        mut self,
        uploader: Box<dyn S3Uploader>,
        base_url: impl Into<String>,
    ) -> Self {
        self.uploader = Some(uploader);
        self.base_url = Some(base_url.into());
        self
    }

    /// Encodes a frame and delivers it, keeping everything the frame knows.
    ///
    /// This is the path the application should take. The [`Exporter`] trait
    /// carries only bytes, which means a clipboard export through it has to
    /// decode them again and loses the colour space on the way; going from the
    /// frame keeps the capture's own metadata all the way to the destination.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Codec`] if encoding failed, [`Error::Io`] if the folder
    /// could not be written, [`Error::Platform`] if the clipboard was
    /// unavailable, or [`Error::Unsupported`] for an S3 destination with no
    /// uploader configured.
    pub fn export_frame(
        &self,
        frame: &Frame,
        format: ImageFormat,
        destination: &Destination,
        context: &NamingContext,
    ) -> Result<ExportOutcome> {
        match destination {
            Destination::Clipboard => Ok(ExportOutcome {
                clipboard: self.write_clipboard(frame)?,
                ..ExportOutcome::default()
            }),
            _ => {
                let bytes = self.encoder.encode(frame, format)?;
                let context = NamingContext {
                    width: frame.width(),
                    height: frame.height(),
                    ..context.clone()
                };
                self.deliver(&bytes, format, destination, &context)
            }
        }
    }

    /// Delivers already-encoded bytes, naming the file from `context`.
    ///
    /// # Errors
    ///
    /// As [`FileExporter::export_frame`].
    pub fn export_bytes(
        &self,
        bytes: &[u8],
        destination: &Destination,
        context: &NamingContext,
    ) -> Result<ExportOutcome> {
        let format = ImageFormat::sniff(bytes).ok_or_else(|| {
            Error::Codec(
                "cannot export: the bytes are not PNG, JPEG or WebP, so no file extension \
                 or media type can be chosen for them"
                    .into(),
            )
        })?;
        self.deliver(bytes, format, destination, context)
    }

    fn deliver(
        &self,
        bytes: &[u8],
        format: ImageFormat,
        destination: &Destination,
        context: &NamingContext,
    ) -> Result<ExportOutcome> {
        match destination {
            Destination::Folder(directory) => {
                let path = self.write_file(bytes, directory, format, context)?;
                Ok(ExportOutcome {
                    url: path_url(&path),
                    path: Some(path),
                    clipboard: None,
                })
            }
            Destination::Clipboard => {
                let frame = decode_to_frame(bytes)?;
                Ok(ExportOutcome {
                    clipboard: self.write_clipboard(&frame)?,
                    ..ExportOutcome::default()
                })
            }
            Destination::S3 { bucket, prefix } => {
                let url = self.upload(bytes, format, bucket, prefix, context)?;
                Ok(ExportOutcome {
                    url: Some(url),
                    ..ExportOutcome::default()
                })
            }
        }
    }

    fn write_clipboard(&self, frame: &Frame) -> Result<Option<ClipboardReport>> {
        self.clipboard.write_image_with_report(frame)
    }

    /// Writes into `directory`, choosing a name nothing else has taken.
    ///
    /// The file is created with `create_new`, so two captures a few milliseconds
    /// apart cannot both win the same name: checking `exists()` and then writing
    /// is a race, and bursts of captures are entirely normal. If the name is
    /// taken between the check and the create, the next candidate is tried
    /// rather than the first capture being overwritten.
    ///
    /// The bytes go straight into that file rather than into a temporary that is
    /// then renamed. A sync client can therefore observe a partially written
    /// file for the millisecond or two the write takes — accepted deliberately,
    /// because the alternative reserves the name with an empty file, and a sync
    /// client that uploads *that* leaves a permanently empty capture in the
    /// cloud, which is worse than a transiently short one.
    fn write_file(
        &self,
        bytes: &[u8],
        directory: &Path,
        format: ImageFormat,
        context: &NamingContext,
    ) -> Result<PathBuf> {
        fs::create_dir_all(directory)?;
        let extension = format.extension();

        for _ in 0..64 {
            let path = self.policy.unique_path(
                directory,
                &self.template,
                context,
                extension,
                &mut |p| p.exists(),
            )?;
            match File::create_new(&path) {
                Ok(mut file) => {
                    file.write_all(bytes)?;
                    // The capture is only safe to report as saved once it is
                    // durable; a queue that retries on error must not retry a
                    // write it was told had succeeded.
                    file.sync_all()?;
                    return Ok(path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(Error::Io(e)),
            }
        }
        Err(Error::Storage(format!(
            "could not find a free filename in {} after 64 attempts",
            directory.display()
        )))
    }

    fn upload(
        &self,
        bytes: &[u8],
        format: ImageFormat,
        bucket: &str,
        prefix: &str,
        context: &NamingContext,
    ) -> Result<String> {
        let uploader = self.uploader.as_ref().ok_or_else(|| Error::Unsupported {
            what: "upload to S3-compatible storage".into(),
            why: "no bucket is configured; add one in settings, or save to a folder — a \
                  synced folder gives you the file everywhere, just not a link"
                .into(),
        })?;

        let name = self
            .policy
            .file_name(&self.template, context, format.extension(), None)?;
        let key = format!("{}{name}", normalise_prefix(prefix));
        let url = uploader.upload(&S3Object {
            bucket,
            key: &key,
            bytes,
            content_type: format.media_type(),
        })?;
        Ok(url)
    }
}

impl Exporter for FileExporter {
    fn export(&self, bytes: &[u8], destination: &Destination) -> Result<Option<String>> {
        let outcome = self.export_bytes(bytes, destination, &NamingContext::now())?;
        Ok(outcome.url)
    }
}

/// Ensures a key prefix ends in exactly one separator, and none leads.
///
/// S3 has no directories; a leading slash produces a bucket with an unnamed
/// top-level folder that most browsers will not show.
fn normalise_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

/// A `file://` URL for a saved capture.
///
/// Not shareable across machines, but it is the honest answer to "where did that
/// go", it is what a terminal will make clickable, and on a network share it is
/// genuinely a link that works for other people.
fn path_url(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    let mut url = String::from("file://");
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                url.push(byte as char);
            }
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    Some(url)
}

/// Recovers a frame from encoded bytes.
///
/// Used only by the byte-oriented [`Exporter`] path. The colour space cannot be
/// recovered without an ICC parser, so it is reported as
/// [`ColorSpace::Unknown`]: claiming sRGB here would be the exact mistake the
/// rest of this crate exists to avoid. Callers that care should use
/// [`FileExporter::export_frame`].
fn decode_to_frame(bytes: &[u8]) -> Result<Frame> {
    let image = image::load_from_memory(bytes)
        .map_err(|e| Error::Codec(format!("could not decode the image to re-encode it: {e}")))?
        .to_rgba8();
    let (width, height) = image.dimensions();
    Ok(Frame {
        stride: width as usize * 4,
        data: image.into_raw(),
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        format: PixelFormat::Rgba8,
        color_space: ColorSpace::Unknown,
        scale: ScaleFactor::IDENTITY,
    })
}
