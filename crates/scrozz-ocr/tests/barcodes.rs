//! Portable decoder tests with fixtures generated entirely in memory.

use rxing::{BarcodeFormat, MultiFormatWriter, Writer, common::BitMatrix};
use scrozz_core::{ColorSpace, Frame, PhysicalSize, PixelFormat, ScaleFactor};
use scrozz_ocr::{BarcodeDetector, BarcodeOptions, PortableBarcodes, Symbology, UpscalePolicy};

fn encoded_frame(payload: &str, format: BarcodeFormat, width: i32, height: i32) -> Frame {
    let matrix = MultiFormatWriter
        .encode(payload, &format, width, height)
        .expect("fixture should encode");
    matrix_frame(&matrix)
}

fn matrix_frame(matrix: &BitMatrix) -> Frame {
    let width = matrix.getWidth();
    let height = matrix.getHeight();
    let mut data = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let sample = if matrix.get(x, y) { 0 } else { 255 };
            data.extend_from_slice(&[sample, sample, sample, 255]);
        }
    }
    Frame {
        data,
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride: width as usize * 4,
        format: PixelFormat::Rgba8,
        color_space: ColorSpace::Srgb,
        scale: ScaleFactor::IDENTITY,
    }
}

fn detector(symbology: Symbology) -> PortableBarcodes {
    PortableBarcodes::with_options(
        BarcodeOptions::new()
            .with_symbologies([symbology])
            .with_upscale(UpscalePolicy::Off),
    )
}

#[test]
fn decodes_a_generated_qr_without_committed_blobs() {
    let payload = "https://example.org/scrozz";
    let frame = encoded_frame(payload, BarcodeFormat::QR_CODE, 240, 240);

    let found = detector(Symbology::QrCode)
        .detect(&frame)
        .expect("QR should decode");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].payload, payload);
    assert_eq!(found[0].symbology, Symbology::QrCode);
    assert!(!found[0].bounds.is_empty());
    assert!(
        found[0].corners.is_empty() || found[0].corners.len() == 4,
        "only a real quadrilateral may be published"
    );
    assert_eq!(
        found[0].link().map(|link| link.target),
        Some(payload.to_string())
    );
}

#[test]
fn decodes_a_generated_linear_barcode() {
    let payload = "SCROZZ-128-042";
    let frame = encoded_frame(payload, BarcodeFormat::CODE_128, 480, 120);

    let found = detector(Symbology::Code128)
        .detect(&frame)
        .expect("Code 128 should decode");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].payload, payload);
    assert_eq!(found[0].symbology, Symbology::Code128);
    assert!(!found[0].bounds.is_empty());
    assert!(found[0].corners.is_empty());
}

#[test]
fn symbology_filter_excludes_other_generated_formats() {
    let frame = encoded_frame("filtered", BarcodeFormat::QR_CODE, 200, 200);
    assert!(
        detector(Symbology::Code128)
            .detect(&frame)
            .expect("a non-match is not an error")
            .is_empty()
    );
}
