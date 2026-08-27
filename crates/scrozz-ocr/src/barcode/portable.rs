//! Pure-Rust barcode detection used on Windows and Linux and as the macOS fallback.

use std::collections::HashSet;

use rxing::common::HybridBinarizer;
use rxing::multi::{GenericMultipleBarcodeReader, MultipleBarcodeReader};
use rxing::{
    BarcodeFormat, BinaryBitmap, DecodeHints, Exceptions, Luma8LuminanceSource, MultiFormatReader,
    RXingResult,
};
use scrozz_core::{Error, Frame, LogicalPoint, Result};

use super::{
    Barcode, BarcodeDetector, BarcodeOptions, Symbology, prepared_pixels_to_logical,
    prepared_point_to_logical,
};
use crate::prepare::{self, Prepared};

/// Barcode detector backed by the pure-Rust `rxing` ZXing port.
#[derive(Debug, Clone, Default)]
pub struct PortableBarcodes {
    options: BarcodeOptions,
}

impl PortableBarcodes {
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

impl BarcodeDetector for PortableBarcodes {
    fn detect(&self, frame: &Frame) -> Result<Vec<Barcode>> {
        let prepared = prepare::prepare(frame, self.options.upscale, None)?;
        let luma = rec601_luma_on_white(&prepared);
        let source = Luma8LuminanceSource::new(luma, prepared.image.width, prepared.image.height)
            .map_err(portable_error)?;
        let mut bitmap = BinaryBitmap::new(HybridBinarizer::new(source));
        let mut reader = GenericMultipleBarcodeReader::new(MultiFormatReader::default());
        let mut hints = DecodeHints {
            TryHarder: Some(true),
            AlsoInverted: Some(true),
            ..DecodeHints::default()
        };

        if !self.options.symbologies.is_empty() {
            let formats = requested_formats(&self.options.symbologies);
            if formats.is_empty() {
                return Ok(Vec::new());
            }
            hints.PossibleFormats = Some(formats);
        }

        let decoded = match reader.decode_multiple_with_hints(&mut bitmap, &hints) {
            Ok(decoded) => decoded,
            Err(Exceptions::NotFoundException(_)) => return Ok(Vec::new()),
            Err(error) => return Err(portable_error(error)),
        };

        let mut barcodes = decoded
            .iter()
            .filter_map(|result| decoded_barcode(result, &prepared, frame, &self.options))
            .collect::<Vec<_>>();
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

fn rec601_luma_on_white(prepared: &Prepared) -> Vec<u8> {
    prepare::rec601_luma_on_white(&prepared.image)
}

fn requested_formats(symbologies: &[Symbology]) -> HashSet<BarcodeFormat> {
    let mut formats = HashSet::new();
    for symbology in symbologies {
        match symbology {
            Symbology::QrCode => {
                formats.insert(BarcodeFormat::QR_CODE);
            }
            Symbology::MicroQrCode => {
                formats.insert(BarcodeFormat::MICRO_QR_CODE);
            }
            Symbology::Aztec => {
                formats.insert(BarcodeFormat::AZTEC);
            }
            Symbology::DataMatrix => {
                formats.insert(BarcodeFormat::DATA_MATRIX);
            }
            Symbology::Pdf417 => {
                formats.insert(BarcodeFormat::PDF_417);
            }
            Symbology::Ean8 => {
                formats.insert(BarcodeFormat::EAN_8);
            }
            Symbology::Ean13 => {
                formats.insert(BarcodeFormat::EAN_13);
                formats.insert(BarcodeFormat::UPC_A);
            }
            Symbology::UpcE => {
                formats.insert(BarcodeFormat::UPC_E);
            }
            Symbology::Code39 => {
                formats.insert(BarcodeFormat::CODE_39);
            }
            Symbology::Code93 => {
                formats.insert(BarcodeFormat::CODE_93);
            }
            Symbology::Code128 => {
                formats.insert(BarcodeFormat::CODE_128);
            }
            Symbology::Codabar => {
                formats.insert(BarcodeFormat::CODABAR);
            }
            Symbology::Itf => {
                formats.insert(BarcodeFormat::ITF);
            }
            Symbology::Other(_) => {}
        }
    }
    formats
}

fn decoded_barcode(
    result: &RXingResult,
    prepared: &Prepared,
    frame: &Frame,
    options: &BarcodeOptions,
) -> Option<Barcode> {
    let symbology = symbology(*result.getBarcodeFormat());
    if !options.accepts(&symbology) {
        return None;
    }

    let points = result
        .getPoints()
        .iter()
        .filter(|point| point.x.is_finite() && point.y.is_finite())
        .collect::<Vec<_>>();
    let bounds = if points.is_empty() {
        Default::default()
    } else {
        let min_x = points
            .iter()
            .map(|point| f64::from(point.x))
            .fold(f64::INFINITY, f64::min);
        let min_y = points
            .iter()
            .map(|point| f64::from(point.y))
            .fold(f64::INFINITY, f64::min);
        let max_x = points
            .iter()
            .map(|point| f64::from(point.x))
            .fold(f64::NEG_INFINITY, f64::max);
        let max_y = points
            .iter()
            .map(|point| f64::from(point.y))
            .fold(f64::NEG_INFINITY, f64::max);
        // Linear readers expose the horizontal scan line, not a quadrilateral.
        // Preserve that real location as a one-prepared-pixel-tall bound rather
        // than publishing an empty rectangle or inventing corners.
        let width = (max_x - min_x).max(1.0);
        let height = (max_y - min_y).max(1.0);
        prepared_pixels_to_logical(min_x, min_y, width, height, prepared, frame)
    };

    let corners = if points.len() == 4 {
        canonical_corners(
            points
                .iter()
                .map(|point| {
                    prepared_point_to_logical(
                        f64::from(point.x),
                        f64::from(point.y),
                        prepared,
                        frame,
                    )
                })
                .collect(),
        )
    } else {
        Vec::new()
    };

    Some(Barcode {
        payload: result.getText().to_owned(),
        symbology,
        bounds,
        corners,
        confidence: 1.0,
    })
}

fn canonical_corners(mut points: Vec<LogicalPoint>) -> Vec<LogicalPoint> {
    if points.len() != 4
        || points.iter().enumerate().any(|(index, point)| {
            points[index + 1..]
                .iter()
                .any(|other| point.x == other.x && point.y == other.y)
        })
    {
        return Vec::new();
    }

    let center_x = points.iter().map(|point| point.x).sum::<f64>() / 4.0;
    let center_y = points.iter().map(|point| point.y).sum::<f64>() / 4.0;
    points.sort_by(|a, b| {
        (a.y - center_y)
            .atan2(a.x - center_x)
            .total_cmp(&(b.y - center_y).atan2(b.x - center_x))
    });
    let top_left = points
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| a.y.total_cmp(&b.y).then_with(|| a.x.total_cmp(&b.x)))
        .map_or(0, |(index, _)| index);
    points.rotate_left(top_left);
    points
}

fn symbology(format: BarcodeFormat) -> Symbology {
    match format {
        BarcodeFormat::QR_CODE => Symbology::QrCode,
        BarcodeFormat::MICRO_QR_CODE => Symbology::MicroQrCode,
        BarcodeFormat::AZTEC => Symbology::Aztec,
        BarcodeFormat::DATA_MATRIX => Symbology::DataMatrix,
        BarcodeFormat::PDF_417 => Symbology::Pdf417,
        BarcodeFormat::EAN_8 => Symbology::Ean8,
        BarcodeFormat::EAN_13 | BarcodeFormat::UPC_A => Symbology::Ean13,
        BarcodeFormat::UPC_E => Symbology::UpcE,
        BarcodeFormat::CODE_39 => Symbology::Code39,
        BarcodeFormat::CODE_93 => Symbology::Code93,
        BarcodeFormat::CODE_128 => Symbology::Code128,
        BarcodeFormat::CODABAR => Symbology::Codabar,
        BarcodeFormat::ITF => Symbology::Itf,
        BarcodeFormat::RECTANGULAR_MICRO_QR_CODE => Symbology::Other("rectangular-micro-qr".into()),
        BarcodeFormat::MAXICODE => Symbology::Other("maxicode".into()),
        BarcodeFormat::RSS_14 => Symbology::Other("rss-14".into()),
        BarcodeFormat::RSS_EXPANDED => Symbology::Other("rss-expanded".into()),
        BarcodeFormat::TELEPEN => Symbology::Other("telepen".into()),
        BarcodeFormat::UPC_EAN_EXTENSION => Symbology::Other("upc-ean-extension".into()),
        BarcodeFormat::DXFilmEdge => Symbology::Other("dx-film-edge".into()),
        BarcodeFormat::UNSUPORTED_FORMAT => Symbology::Other("unsupported".into()),
    }
}

fn portable_error(error: Exceptions) -> Error {
    Error::Platform(format!("portable barcode detector failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{PhysicalSize, PixelFormat, ScaleFactor};

    fn prepared_pixel(rgba: [u8; 4]) -> Prepared {
        Prepared {
            image: prepare::Rgba8Image::new(1, 1, rgba.to_vec()).expect("pixel"),
            upscale: 1.0,
            source_size: PhysicalSize::new(1.0, 1.0),
        }
    }

    #[test]
    fn rec601_uses_all_colour_channels() {
        assert_eq!(
            rec601_luma_on_white(&prepared_pixel([255, 0, 0, 255])),
            vec![76]
        );
        assert_eq!(
            rec601_luma_on_white(&prepared_pixel([0, 255, 0, 255])),
            vec![150]
        );
        assert_eq!(
            rec601_luma_on_white(&prepared_pixel([0, 0, 255, 255])),
            vec![29]
        );
    }

    #[test]
    fn transparent_pixels_are_composited_onto_white() {
        assert_eq!(
            rec601_luma_on_white(&prepared_pixel([0, 0, 0, 0])),
            vec![255]
        );
        assert_eq!(
            rec601_luma_on_white(&prepared_pixel([255, 255, 255, 0])),
            vec![255]
        );
    }

    #[test]
    fn corners_are_not_invented() {
        assert!(
            canonical_corners(vec![
                LogicalPoint::new(0.0, 0.0),
                LogicalPoint::new(1.0, 0.0),
                LogicalPoint::new(1.0, 1.0),
            ])
            .is_empty()
        );
    }

    #[test]
    fn four_real_corners_are_ordered_from_top_left() {
        let corners = canonical_corners(vec![
            LogicalPoint::new(8.0, 9.0),
            LogicalPoint::new(2.0, 1.0),
            LogicalPoint::new(1.0, 8.0),
            LogicalPoint::new(9.0, 2.0),
        ]);
        assert_eq!(corners[0], LogicalPoint::new(2.0, 1.0));
        assert_eq!(corners[1], LogicalPoint::new(9.0, 2.0));
        assert_eq!(corners[2], LogicalPoint::new(8.0, 9.0));
        assert_eq!(corners[3], LogicalPoint::new(1.0, 8.0));
    }

    #[test]
    fn malformed_frames_fail_before_the_decoder() {
        let frame = Frame {
            data: Vec::new(),
            size: PhysicalSize::new(2.0, 2.0),
            stride: 8,
            format: PixelFormat::Rgba8,
            color_space: Default::default(),
            scale: ScaleFactor::IDENTITY,
        };
        assert!(matches!(
            PortableBarcodes::new().detect(&frame),
            Err(Error::InvalidRequest(_))
        ));
    }
}
