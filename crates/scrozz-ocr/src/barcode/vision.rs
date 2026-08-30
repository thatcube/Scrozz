//! Native macOS barcode detection through Vision.

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSDictionary, NSString};
use objc2_vision::{VNDetectBarcodesRequest, VNImageRequestHandler, VNRequest};
use scrozz_core::{Error, Frame, LogicalPoint, PhysicalPoint, Result};

use super::{Barcode, BarcodeDetector, BarcodeOptions, Symbology};
use crate::layout::{self, NormalizedRect};
use crate::prepare;

/// Barcode detector backed by `VNDetectBarcodesRequest`.
#[derive(Debug, Clone, Default)]
pub struct VisionBarcodes {
    options: BarcodeOptions,
}

impl VisionBarcodes {
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
}

impl BarcodeDetector for VisionBarcodes {
    fn detect(&self, frame: &Frame) -> Result<Vec<Barcode>> {
        let prepared = prepare::prepare(frame, self.options.upscale, None)?;
        let image = crate::macos::cg_image(&prepared)?;
        // SAFETY: this is the ordinary Objective-C `+new` constructor.
        let request = unsafe { VNDetectBarcodesRequest::new() };
        if !configure(&request, &self.options)? {
            return Ok(Vec::new());
        }

        // SAFETY: `image` is a live CGImage and the options dictionary is valid
        // for the duration of the synchronous request.
        let handler = unsafe {
            VNImageRequestHandler::initWithCGImage_options(
                VNImageRequestHandler::alloc(),
                &image,
                &NSDictionary::new(),
            )
        };
        let requests: Retained<NSArray<VNRequest>> = NSArray::from_slice(&[request.as_ref()]);
        handler.performRequests_error(&requests).map_err(|error| {
            Error::Platform(format!("Vision barcode detection failed: {error}"))
        })?;

        // SAFETY: the request is live and has completed synchronously.
        let Some(observations) = (unsafe { request.results() }) else {
            return Ok(Vec::new());
        };

        let mut barcodes = Vec::with_capacity(observations.len());
        for observation in &observations {
            // SAFETY: all properties are read from a live observation returned by
            // this completed request.
            let Some(payload) = (unsafe { observation.payloadStringValue() }) else {
                continue;
            };
            let payload = payload.to_string();
            if payload.is_empty() {
                continue;
            }
            let native_symbology = unsafe { observation.symbology() }.to_string();
            let symbology = from_vision_symbology(&native_symbology);
            if !self.options.accepts(&symbology) {
                continue;
            }

            let bb = unsafe { observation.boundingBox() };
            let normalized =
                NormalizedRect::new(bb.origin.x, bb.origin.y, bb.size.width, bb.size.height);
            let physical =
                layout::bottom_left_normalized_to_physical(normalized, prepared.source_size);
            if physical.is_empty() {
                continue;
            }

            let corners = vision_corners(&observation, prepared.source_size, frame);
            barcodes.push(Barcode {
                payload,
                symbology,
                bounds: layout::to_logical(physical, frame.scale),
                corners,
                confidence: unsafe { observation.confidence() }.clamp(0.0, 1.0),
            });
        }

        barcodes.sort_by(|a, b| {
            a.bounds
                .origin
                .y
                .total_cmp(&b.bounds.origin.y)
                .then_with(|| a.bounds.origin.x.total_cmp(&b.bounds.origin.x))
                .then_with(|| a.symbology.token().cmp(b.symbology.token()))
                .then_with(|| a.payload.cmp(&b.payload))
        });
        Ok(barcodes)
    }
}

/// Configures only symbologies supported by this OS revision.
///
/// Vision's symbologies are typed strings. Constructing those strings directly
/// avoids a hard link to newer SDK constants while the supported-list
/// intersection prevents an older OS from receiving a value it does not know.
fn configure(request: &VNDetectBarcodesRequest, options: &BarcodeOptions) -> Result<bool> {
    if options.symbologies.is_empty() {
        return Ok(true);
    }

    // SAFETY: this reads the capabilities of a live, newly created request.
    let supported = unsafe { request.supportedSymbologiesAndReturnError() }
        .map_err(|error| Error::Platform(format!("Vision barcode capabilities failed: {error}")))?;
    let supported = supported
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>();
    let requested = options
        .symbologies
        .iter()
        .flat_map(|symbology| vision_names(symbology).iter().copied())
        .filter(|name| supported.iter().any(|item| item == *name))
        .map(NSString::from_str)
        .collect::<Vec<_>>();
    if requested.is_empty() {
        return Ok(false);
    }

    let requested = NSArray::from_retained_slice(&requested);
    // SAFETY: every string came from Vision's supported list for this request.
    unsafe { request.setSymbologies(&requested) };
    Ok(true)
}

fn vision_names(symbology: &Symbology) -> &'static [&'static str] {
    match symbology {
        Symbology::QrCode => &["VNBarcodeSymbologyQR"],
        Symbology::MicroQrCode => &["VNBarcodeSymbologyMicroQR"],
        Symbology::RectangularMicroQrCode => &[],
        Symbology::Aztec => &["VNBarcodeSymbologyAztec"],
        Symbology::DataMatrix => &["VNBarcodeSymbologyDataMatrix"],
        Symbology::Pdf417 => &["VNBarcodeSymbologyPDF417", "VNBarcodeSymbologyMicroPDF417"],
        Symbology::Ean8 => &["VNBarcodeSymbologyEAN8"],
        Symbology::Ean13 => &["VNBarcodeSymbologyEAN13"],
        Symbology::UpcE => &["VNBarcodeSymbologyUPCE"],
        Symbology::Code39 => &[
            "VNBarcodeSymbologyCode39",
            "VNBarcodeSymbologyCode39Checksum",
            "VNBarcodeSymbologyCode39FullASCII",
            "VNBarcodeSymbologyCode39FullASCIIChecksum",
        ],
        Symbology::Code93 => &["VNBarcodeSymbologyCode93", "VNBarcodeSymbologyCode93i"],
        Symbology::Code128 => &["VNBarcodeSymbologyCode128"],
        Symbology::Codabar => &["VNBarcodeSymbologyCodabar"],
        Symbology::Itf => &[
            "VNBarcodeSymbologyI2of5",
            "VNBarcodeSymbologyI2of5Checksum",
            "VNBarcodeSymbologyITF14",
        ],
        Symbology::Other(_) => &[],
    }
}

fn from_vision_symbology(name: &str) -> Symbology {
    match name {
        "VNBarcodeSymbologyQR" => Symbology::QrCode,
        "VNBarcodeSymbologyMicroQR" => Symbology::MicroQrCode,
        "VNBarcodeSymbologyAztec" => Symbology::Aztec,
        "VNBarcodeSymbologyDataMatrix" => Symbology::DataMatrix,
        "VNBarcodeSymbologyPDF417" | "VNBarcodeSymbologyMicroPDF417" => Symbology::Pdf417,
        "VNBarcodeSymbologyEAN8" => Symbology::Ean8,
        "VNBarcodeSymbologyEAN13" => Symbology::Ean13,
        "VNBarcodeSymbologyUPCE" => Symbology::UpcE,
        "VNBarcodeSymbologyCode39"
        | "VNBarcodeSymbologyCode39Checksum"
        | "VNBarcodeSymbologyCode39FullASCII"
        | "VNBarcodeSymbologyCode39FullASCIIChecksum" => Symbology::Code39,
        "VNBarcodeSymbologyCode93" | "VNBarcodeSymbologyCode93i" => Symbology::Code93,
        "VNBarcodeSymbologyCode128" => Symbology::Code128,
        "VNBarcodeSymbologyCodabar" => Symbology::Codabar,
        "VNBarcodeSymbologyI2of5"
        | "VNBarcodeSymbologyI2of5Checksum"
        | "VNBarcodeSymbologyITF14" => Symbology::Itf,
        other => Symbology::Other(vision_token(other)),
    }
}

fn vision_token(name: &str) -> String {
    name.strip_prefix("VNBarcodeSymbology")
        .unwrap_or(name)
        .chars()
        .enumerate()
        .fold(String::new(), |mut token, (index, character)| {
            if index > 0 && character.is_ascii_uppercase() {
                token.push('-');
            }
            token.push(character.to_ascii_lowercase());
            token
        })
}

fn vision_corners(
    observation: &objc2_vision::VNBarcodeObservation,
    source_size: scrozz_core::PhysicalSize,
    frame: &Frame,
) -> Vec<LogicalPoint> {
    // SAFETY: the observation is live. These are the four actual quadrilateral
    // points supplied by Vision, not corners synthesized from its bounding box.
    let native = unsafe {
        [
            observation.topLeft(),
            observation.topRight(),
            observation.bottomRight(),
            observation.bottomLeft(),
        ]
    };
    if native
        .iter()
        .any(|point| !point.x.is_finite() || !point.y.is_finite())
    {
        return Vec::new();
    }

    let points = native.map(|point| {
        let physical = PhysicalPoint::new(
            point.x * source_size.width,
            (1.0 - point.y) * source_size.height,
        );
        LogicalPoint::new(
            physical.x / frame.scale.get(),
            physical.y / frame.scale.get(),
        )
    });
    if points.iter().enumerate().any(|(index, point)| {
        points[index + 1..]
            .iter()
            .any(|other| point.x == other.x && point.y == other.y)
    }) {
        return Vec::new();
    }
    points.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vision_names_map_to_public_symbologies() {
        assert_eq!(
            from_vision_symbology("VNBarcodeSymbologyQR"),
            Symbology::QrCode
        );
        assert_eq!(
            from_vision_symbology("VNBarcodeSymbologyCode39FullASCII"),
            Symbology::Code39
        );
        assert_eq!(
            from_vision_symbology("VNBarcodeSymbologyI2of5Checksum"),
            Symbology::Itf
        );
    }

    #[test]
    fn future_vision_names_remain_identifiable() {
        assert_eq!(
            from_vision_symbology("VNBarcodeSymbologyFooBar"),
            Symbology::Other("foo-bar".into())
        );
    }
}
