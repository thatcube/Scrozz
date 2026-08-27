//! Encoders, clipboard flavours, and destinations.

#![forbid(unsafe_code)]

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
