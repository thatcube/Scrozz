//! Encoders, clipboard flavours, and destinations.

#![forbid(unsafe_code)]

pub mod clipboard;
pub mod decode;
pub mod destination;
pub mod encode;
pub mod icc;
pub mod naming;
pub mod pixels;

pub use clipboard::{
    ClipboardPlatform, ClipboardReport, Flavour, FlavourGap, FlavourKind, SystemClipboard,
};
pub use decode::{decode, decode_file};
pub use destination::{ExportOutcome, FileExporter, S3Object, S3Uploader, UnimplementedS3Uploader};
pub use encode::{EncodeOptions, FrameEncoder, PngEffort};
pub use icc::profile_for;
pub use naming::{FilenameRules, NamePolicy, NameTemplate, NamingContext, Timestamp};
pub use pixels::{RgbaImage, to_straight_rgba8};

use std::path::PathBuf;

use scrozz_core::{Frame, Result};

/// An output image format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// Lossless, alpha-capable. The default.
    Png,
    /// Lossy, no alpha.
    Jpeg,
    /// Lossy or lossless, alpha-capable, much smaller than PNG.
    WebP,
}

impl ImageFormat {
    /// The conventional file extension, without a dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::WebP => "webp",
        }
    }

    /// The IANA media type.
    ///
    /// Needed when uploading: an object stored without one is served as
    /// `application/octet-stream`, and a shared link then downloads a file
    /// instead of showing a picture, which defeats the point of sharing it.
    #[must_use]
    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::WebP => "image/webp",
        }
    }

    /// Whether this format can carry transparency.
    ///
    /// JPEG cannot, which is why encoding to it composites over a background
    /// colour rather than discarding alpha and letting the colour under a
    /// transparent pixel show through as whatever noise the buffer held.
    #[must_use]
    pub const fn supports_alpha(self) -> bool {
        matches!(self, Self::Png | Self::WebP)
    }

    /// Identifies a format from its leading bytes.
    ///
    /// The [`Exporter`] contract receives bytes with no accompanying format, but
    /// a file still needs an extension and an upload still needs a media type.
    /// Sniffing is how those are recovered without changing the contract.
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some(Self::Jpeg)
        } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            Some(Self::WebP)
        } else {
            None
        }
    }
}

/// Writes frames to bytes.
pub trait Encoder {
    /// Encodes a frame.
    ///
    /// Implementations must honour [`scrozz_core::Frame::color_space`] by
    /// embedding the matching profile. Dropping it makes every wide-gamut
    /// capture look washed out in some viewers and oversaturated in others,
    /// which reads as "this app produces bad screenshots".
    ///
    /// # Errors
    ///
    /// Returns [`scrozz_core::Error::Codec`] if encoding failed.
    fn encode(&self, frame: &Frame, format: ImageFormat) -> Result<Vec<u8>>;
}

/// Places captures on the system clipboard.
pub trait Clipboard {
    /// Writes a frame in every flavour the platform can offer.
    ///
    /// Per decision D10 this offers multiple representations at once — PNG for
    /// fidelity, and the platform's native bitmap flavour for the many apps that
    /// accept nothing else. Offering only PNG means pasting silently fails in
    /// exactly the older Office and chat clients people most often paste into.
    ///
    /// # Errors
    ///
    /// Returns an error if the clipboard was unavailable.
    fn write_image(&self, frame: &Frame) -> Result<()>;

    /// Writes a frame and reports which flavours were actually delivered.
    ///
    /// Separate from [`Clipboard::write_image`] and defaulted, because what a
    /// backend manages to offer is genuinely backend-specific and worth
    /// surfacing rather than assuming. The default answers "no report
    /// available" instead of inventing an optimistic one.
    ///
    /// # Errors
    ///
    /// Returns an error if the clipboard was unavailable.
    fn write_image_with_report(&self, frame: &Frame) -> Result<Option<clipboard::ClipboardReport>> {
        self.write_image(frame)?;
        Ok(None)
    }
}

/// Where an export is sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Destination {
    /// Any folder the user chose.
    ///
    /// Per decision D18 this is genuinely any folder, which lets a Dropbox,
    /// iCloud or Syncthing directory provide sync for free without Scrozz
    /// running a service.
    Folder(PathBuf),
    /// The system clipboard.
    Clipboard,
    /// An S3-compatible bucket, for shareable links.
    ///
    /// The one thing a folder cannot do is produce a URL. Using S3-compatible
    /// storage rather than a hosted Scrozz service means links cost the project
    /// nothing, cannot be shut off, and keep the user owning their own data.
    S3 {
        /// Bucket name.
        bucket: String,
        /// Key prefix within the bucket.
        prefix: String,
    },
}

/// Delivers encoded bytes to a destination.
pub trait Exporter {
    /// Sends bytes to a destination, returning a shareable URL when one exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the destination rejected the write.
    fn export(&self, bytes: &[u8], destination: &Destination) -> Result<Option<String>>;
}
