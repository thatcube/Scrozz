//! QR codes and barcodes (OCR-04).
//!
//! # Why this belongs next to text recognition
//!
//! A QR code in a screenshot is a URL the user cannot click and will not retype.
//! It is the same job as OCR — *get the thing out of the picture and onto the
//! clipboard* — so it lives behind the same command and returns the same shape
//! of answer: a payload **and its bounds**, so a UI can point at what it found
//! rather than dumping a string with no provenance.
//!
//! # Two implementations, deliberately
//!
//! [`VisionBarcodes`] wraps `VNDetectBarcodesRequest` on macOS. It is the same
//! detector the camera app uses: better on skewed, low-contrast and partially
//! occluded codes than anything that could be bundled, and free.
//!
//! [`PortableBarcodes`] is a pure-Rust ZXing port. It is what Windows and Linux
//! use, because neither ships a system barcode API — and it is compiled on
//! **every** platform on purpose, not `#[cfg]`-ed away on macOS. Two reasons:
//!
//! 1. A decoder that only runs on two of three platforms is only *tested* on
//!    two of three. Compiling it everywhere means the maintainer's macOS machine
//!    and every CI runner exercise the Windows/Linux path on every commit.
//! 2. It is a real fallback. Vision can fail for reasons that have nothing to do
//!    with the image — a request revision the OS rejects, a transient handler
//!    failure — and returning "no codes found" then would be a lie.
//!
//! [`SystemBarcodes`] is the one callers should use: Vision first on macOS,
//! portable everywhere else and whenever Vision comes back empty-handed.
//!
//! # Nothing leaves the machine
//!
//! Both paths are local. Decoding a QR does **not** fetch the URL it contains,
//! and no payload is resolved, expanded or previewed. A barcode's payload is
//! attacker-controlled text that arrived in a screenshot; it is reported, never
//! acted upon.

use scrozz_core::{Frame, LogicalPoint, LogicalRect, Result};

use crate::layout;
use crate::links::{self, Link};
use crate::prepare::{self, UpscalePolicy};

mod portable;

pub use portable::PortableBarcodes;

#[cfg(target_os = "macos")]
mod vision;

#[cfg(target_os = "macos")]
pub use vision::VisionBarcodes;

/// A barcode family.
///
/// The variants are the ones the two detectors agree on. Anything else is
/// reported as [`Symbology::Other`] with the engine's own name rather than
/// silently mapped onto a neighbour, because "this is a Code 93" and "this is
/// something we do not have a name for" are different facts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Symbology {
    /// QR code.
    QrCode,
    /// Micro QR code.
    MicroQrCode,
    /// Aztec.
    Aztec,
    /// Data Matrix.
    DataMatrix,
    /// PDF417.
    Pdf417,
    /// EAN-8.
    Ean8,
    /// EAN-13. Vision reports UPC-A as EAN-13, which is what the standard says
    /// it is: a UPC-A is an EAN-13 with a leading zero.
    Ean13,
    /// UPC-E.
    UpcE,
    /// Code 39.
    Code39,
    /// Code 93.
    Code93,
    /// Code 128.
    Code128,
    /// Codabar.
    Codabar,
    /// Interleaved 2 of 5.
    Itf,
    /// A symbology this build has no name for. Carries the engine's own label.
    Other(String),
}

impl Symbology {
    /// The stable token used in CLI output and accepted by `--symbology`.
    ///
    /// Lowercase and hyphenated, and **part of the scripting contract** (D11):
    /// these strings are matched by shell scripts, so they change only with the
    /// same care as a command-line flag.
    #[must_use]
    pub fn token(&self) -> &str {
        match self {
            Self::QrCode => "qr",
            Self::MicroQrCode => "micro-qr",
            Self::Aztec => "aztec",
            Self::DataMatrix => "data-matrix",
            Self::Pdf417 => "pdf417",
            Self::Ean8 => "ean-8",
            Self::Ean13 => "ean-13",
            Self::UpcE => "upc-e",
            Self::Code39 => "code-39",
            Self::Code93 => "code-93",
            Self::Code128 => "code-128",
            Self::Codabar => "codabar",
            Self::Itf => "itf",
            Self::Other(name) => name,
        }
    }

    /// Parses a token produced by [`Self::token`].
    ///
    /// Unknown tokens become [`Symbology::Other`] rather than an error: a filter
    /// naming a symbology this build does not know simply matches nothing, which
    /// is a better outcome for a script than a hard failure.
    #[must_use]
    pub fn parse(token: &str) -> Self {
        let lower = token.trim().to_ascii_lowercase();
        match lower.as_str() {
            "qr" | "qrcode" | "qr-code" => Self::QrCode,
            "micro-qr" | "microqr" => Self::MicroQrCode,
            "aztec" => Self::Aztec,
            "data-matrix" | "datamatrix" => Self::DataMatrix,
            "pdf417" | "pdf-417" => Self::Pdf417,
            "ean-8" | "ean8" => Self::Ean8,
            "ean-13" | "ean13" | "upc-a" | "upca" => Self::Ean13,
            "upc-e" | "upce" => Self::UpcE,
            "code-39" | "code39" => Self::Code39,
            "code-93" | "code93" => Self::Code93,
            "code-128" | "code128" => Self::Code128,
            "codabar" => Self::Codabar,
            "itf" | "i2of5" | "interleaved-2-of-5" => Self::Itf,
            _ => Self::Other(lower),
        }
    }

    /// Whether this is a 2D (matrix) symbology rather than a linear one.
    ///
    /// A UI draws a square highlight around one and a wide flat one around the
    /// other; getting it backwards looks broken even when the payload is right.
    #[must_use]
    pub const fn is_matrix(&self) -> bool {
        matches!(
            self,
            Self::QrCode | Self::MicroQrCode | Self::Aztec | Self::DataMatrix | Self::Pdf417
        )
    }

    /// Every symbology this build can name, for `--help` and settings UI.
    #[must_use]
    pub fn all() -> Vec<Self> {
        vec![
            Self::QrCode,
            Self::MicroQrCode,
            Self::Aztec,
            Self::DataMatrix,
            Self::Pdf417,
            Self::Ean8,
            Self::Ean13,
            Self::UpcE,
            Self::Code39,
            Self::Code93,
            Self::Code128,
            Self::Codabar,
            Self::Itf,
        ]
    }
}

/// One decoded barcode.
#[derive(Debug, Clone, PartialEq)]
pub struct Barcode {
    /// The decoded payload, exactly as encoded.
    ///
    /// Untrusted text. It is displayed and copied; it is never fetched, and a
    /// UI that renders it must not turn it into a live link without the user
    /// asking — see [`Barcode::link`].
    pub payload: String,
    /// Which symbology it was.
    pub symbology: Symbology,
    /// Where it sits in the frame, in top-left logical points.
    pub bounds: LogicalRect,
    /// The four corners, when the detector reports them, in the order
    /// top-left, top-right, bottom-right, bottom-left.
    ///
    /// Empty when the detector gives only an axis-aligned box. Non-empty is
    /// worth having: a QR photographed at an angle has a genuinely rotated
    /// quadrilateral, and a UI that only knows the bounding box draws a
    /// highlight visibly larger than the code.
    pub corners: Vec<LogicalPoint>,
    /// Detector confidence, 0.0 to 1.0. Engines that do not report one use 1.0,
    /// which is honest: a barcode either checksums or it does not.
    pub confidence: f32,
}

impl Barcode {
    /// The payload as a link, when it is one.
    ///
    /// Returns `None` for a product code or arbitrary text. A `Some` result is
    /// still not permission to open anything — it is what the UI needs to *offer*
    /// opening it.
    #[must_use]
    pub fn link(&self) -> Option<Link> {
        links::classify(&self.payload).map(|kind| Link {
            text: self.payload.clone(),
            target: kind.target(&self.payload),
            kind,
            bounds: self.bounds,
            block: None,
        })
    }
}

/// Tuning for barcode detection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BarcodeOptions {
    /// Restrict detection to these symbologies. Empty means every one the
    /// detector supports, which is the right default: a user scanning a
    /// screenshot does not know what kind of code is in it.
    pub symbologies: Vec<Symbology>,
    /// Whether to enlarge small images first.
    ///
    /// Defaults to [`UpscalePolicy::Automatic`] for the same reason text does: a
    /// 90-pixel QR in a 1× screenshot is below what either detector locates
    /// reliably, and resampling is far cheaper than a failed scan.
    pub upscale: UpscalePolicy,
}

impl BarcodeOptions {
    /// Default options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restricts detection to the given symbologies.
    #[must_use]
    pub fn with_symbologies<I: IntoIterator<Item = Symbology>>(mut self, symbologies: I) -> Self {
        self.symbologies = symbologies.into_iter().collect();
        self
    }

    /// Sets the upscale policy.
    #[must_use]
    pub const fn with_upscale(mut self, upscale: UpscalePolicy) -> Self {
        self.upscale = upscale;
        self
    }

    /// Whether a symbology passes the filter.
    #[must_use]
    pub fn accepts(&self, symbology: &Symbology) -> bool {
        self.symbologies.is_empty() || self.symbologies.contains(symbology)
    }
}

/// Finds barcodes in an image.
pub trait BarcodeDetector {
    /// Detects every barcode in a frame.
    ///
    /// An image with no barcode returns an empty vector — that is an ordinary
    /// outcome, not an error.
    ///
    /// # Errors
    ///
    /// Returns [`scrozz_core::Error::InvalidRequest`] for a malformed frame, or
    /// [`scrozz_core::Error::Platform`] if the underlying detector fails.
    fn detect(&self, frame: &Frame) -> Result<Vec<Barcode>>;
}

/// The best barcode detector available on this platform.
///
/// Vision on macOS, falling back to the portable decoder; the portable decoder
/// alone on Windows and Linux. Unlike text recognition there is **no**
/// unsupported platform: the pure-Rust decoder works everywhere, so
/// `scrozz barcodes` is one of the few capabilities that behaves identically on
/// all three targets.
#[derive(Debug, Clone, Default)]
pub struct SystemBarcodes {
    options: BarcodeOptions,
}

impl SystemBarcodes {
    /// Creates a detector with default options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a detector with explicit options.
    #[must_use]
    pub const fn with_options(options: BarcodeOptions) -> Self {
        Self { options }
    }

    /// The options in force.
    #[must_use]
    pub const fn options(&self) -> &BarcodeOptions {
        &self.options
    }

    /// Whether this build can detect barcodes at all. Always true — kept so a
    /// UI can ask the same question of every capability.
    #[must_use]
    pub const fn is_available() -> bool {
        true
    }

    /// The name of the detector that will run first, for diagnostics.
    #[must_use]
    pub const fn engine_name() -> &'static str {
        if cfg!(target_os = "macos") {
            "vision"
        } else {
            "portable"
        }
    }
}

impl BarcodeDetector for SystemBarcodes {
    fn detect(&self, frame: &Frame) -> Result<Vec<Barcode>> {
        #[cfg(target_os = "macos")]
        {
            match VisionBarcodes::with_options(self.options.clone()).detect(frame) {
                // Vision found nothing. Not necessarily an empty image: it is
                // stricter about low-contrast linear codes than ZXing, so a
                // second opinion is worth the milliseconds.
                Ok(found) if found.is_empty() => {}
                other => return other,
            }
        }
        PortableBarcodes::with_options(self.options.clone()).detect(frame)
    }
}

/// Converts a detector's rectangle in prepared-image pixels into frame logical
/// points.
///
/// Shared by both backends so the upscale division happens in exactly one
/// place. It is the division everyone forgets, and it is invisible on a Retina
/// machine where the factor is 1.0 and wrong by 2× on a 1× monitor.
pub(crate) fn prepared_pixels_to_logical(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    prepared: &prepare::Prepared,
    frame: &Frame,
) -> LogicalRect {
    let physical = layout::pixels_to_physical(
        x,
        y,
        width,
        height,
        prepared.upscale,
        prepared.source_size,
    );
    layout::to_logical(physical, frame.scale)
}

/// Converts a point in prepared-image pixels into a frame logical point.
pub(crate) fn prepared_point_to_logical(
    x: f64,
    y: f64,
    prepared: &prepare::Prepared,
    frame: &Frame,
) -> LogicalPoint {
    let rect = prepared_pixels_to_logical(x, y, 0.0, 0.0, prepared, frame);
    rect.origin
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip_through_parse() {
        for symbology in Symbology::all() {
            assert_eq!(
                Symbology::parse(symbology.token()),
                symbology,
                "{} must survive a round trip; the token is a scripting contract",
                symbology.token()
            );
        }
    }

    #[test]
    fn upc_a_is_accepted_as_a_spelling_of_ean_13() {
        // Vision reports a UPC-A as EAN-13 and it is not wrong to, so a user who
        // types the name on the packet must still get a match.
        assert_eq!(Symbology::parse("upc-a"), Symbology::Ean13);
    }

    #[test]
    fn an_unknown_token_is_named_rather_than_rejected() {
        assert_eq!(
            Symbology::parse("MSI-Plessey"),
            Symbology::Other("msi-plessey".into())
        );
    }

    #[test]
    fn matrix_and_linear_are_not_confused() {
        assert!(Symbology::QrCode.is_matrix());
        assert!(Symbology::Pdf417.is_matrix());
        assert!(!Symbology::Code128.is_matrix());
        assert!(!Symbology::Ean13.is_matrix());
    }

    #[test]
    fn an_empty_filter_accepts_everything() {
        let options = BarcodeOptions::new();
        assert!(Symbology::all().iter().all(|s| options.accepts(s)));
    }

    #[test]
    fn a_filter_admits_only_what_it_names() {
        let options = BarcodeOptions::new().with_symbologies([Symbology::QrCode]);
        assert!(options.accepts(&Symbology::QrCode));
        assert!(!options.accepts(&Symbology::Code128));
    }

    #[test]
    fn a_url_payload_becomes_an_offerable_link() {
        let code = Barcode {
            payload: "https://example.org/a".into(),
            symbology: Symbology::QrCode,
            bounds: LogicalRect::default(),
            corners: Vec::new(),
            confidence: 1.0,
        };
        let link = code.link().expect("a https payload is a link");
        assert_eq!(link.target, "https://example.org/a");
    }

    #[test]
    fn a_product_code_payload_is_not_a_link() {
        let code = Barcode {
            payload: "0123456789012".into(),
            symbology: Symbology::Ean13,
            bounds: LogicalRect::default(),
            corners: Vec::new(),
            confidence: 1.0,
        };
        assert!(
            code.link().is_none(),
            "a product number must not be offered as something to open"
        );
    }
}
