//! Portable decoder tests with fixtures generated entirely in memory.

use rxing::{BarcodeFormat, MultiFormatWriter, Writer, common::BitMatrix};
use scrozz_core::{ColorSpace, Frame, PhysicalSize, PixelFormat, ScaleFactor};
use scrozz_ocr::{BarcodeDetector, BarcodeOptions, PortableBarcodes, Symbology, UpscalePolicy};

fn encoded_frame(payload: &str, format: BarcodeFormat, width: i32, height: i32) -> Frame {
    matrix_frame(&encoded_matrix(payload, format, width, height))
}

fn encoded_matrix(payload: &str, format: BarcodeFormat, width: i32, height: i32) -> BitMatrix {
    MultiFormatWriter
        .encode(payload, &format, width, height)
        .expect("fixture should encode")
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

fn inverted_frame(matrix: &BitMatrix) -> Frame {
    let width = matrix.getWidth();
    let height = matrix.getHeight();
    let mut data = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let sample = if matrix.get(x, y) { 255 } else { 0 };
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

fn rotated_matrix(matrix: &BitMatrix, radians: f64, padding: u32) -> BitMatrix {
    let source_width = f64::from(matrix.getWidth());
    let source_height = f64::from(matrix.getHeight());
    let cosine = radians.cos();
    let sine = radians.sin();
    let width = (source_width * cosine.abs() + source_height * sine.abs()).ceil() as u32;
    let height = (source_width * sine.abs() + source_height * cosine.abs()).ceil() as u32;
    let mut rotated =
        BitMatrix::new(width + 2 * padding, height + 2 * padding).expect("rotated fixture");
    let source_center = ((source_width - 1.0) / 2.0, (source_height - 1.0) / 2.0);
    let target_center = (
        (f64::from(rotated.getWidth()) - 1.0) / 2.0,
        (f64::from(rotated.getHeight()) - 1.0) / 2.0,
    );

    for y in 0..rotated.getHeight() {
        for x in 0..rotated.getWidth() {
            let translated_x = f64::from(x) - target_center.0;
            let translated_y = f64::from(y) - target_center.1;
            let source_x = cosine.mul_add(translated_x, sine * translated_y) + source_center.0;
            let source_y = (-sine).mul_add(translated_x, cosine * translated_y) + source_center.1;
            let source_x = source_x.round() as i32;
            let source_y = source_y.round() as i32;
            if source_x >= 0
                && source_y >= 0
                && source_x < matrix.getWidth() as i32
                && source_y < matrix.getHeight() as i32
                && matrix.get(source_x as u32, source_y as u32)
            {
                rotated.set(x, y);
            }
        }
    }
    rotated
}

fn composite_frame(width: u32, height: u32, symbols: &[(&BitMatrix, u32, u32)]) -> Frame {
    let mut data = vec![255; width as usize * height as usize * 4];
    for alpha in data.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }
    for (matrix, left, top) in symbols {
        for y in 0..matrix.getHeight() {
            for x in 0..matrix.getWidth() {
                if matrix.get(x, y) {
                    let offset = ((top + y) * width + left + x) as usize * 4;
                    data[offset..offset + 3].fill(0);
                }
            }
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

fn ink_bounds(matrix: &BitMatrix) -> (f64, f64, f64, f64) {
    let mut left = matrix.getWidth();
    let mut top = matrix.getHeight();
    let mut right = 0;
    let mut bottom = 0;
    for y in 0..matrix.getHeight() {
        for x in 0..matrix.getWidth() {
            if matrix.get(x, y) {
                left = left.min(x);
                top = top.min(y);
                right = right.max(x + 1);
                bottom = bottom.max(y + 1);
            }
        }
    }
    (
        f64::from(left),
        f64::from(top),
        f64::from(right),
        f64::from(bottom),
    )
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
    let matrix = encoded_matrix(payload, BarcodeFormat::QR_CODE, 240, 240);
    let frame = matrix_frame(&matrix);

    let found = detector(Symbology::QrCode)
        .detect(&frame)
        .expect("QR should decode");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].payload, payload);
    assert_eq!(found[0].symbology, Symbology::QrCode);
    assert!(!found[0].bounds.is_empty());
    assert_eq!(found[0].corners.len(), 4);
    let (left, top, right, bottom) = ink_bounds(&matrix);
    let bounds = found[0].bounds;
    assert!((bounds.origin.x - left).abs() <= 2.0, "{bounds:?}");
    assert!((bounds.origin.y - top).abs() <= 2.0, "{bounds:?}");
    assert!(
        (bounds.origin.x + bounds.size.width - right).abs() <= 2.0,
        "{bounds:?}"
    );
    assert!(
        (bounds.origin.y + bounds.size.height - bottom).abs() <= 2.0,
        "{bounds:?}"
    );
    assert_eq!(
        found[0].link().map(|link| link.target),
        Some(payload.to_string())
    );
}

#[test]
fn decodes_a_generated_linear_barcode() {
    let payload = "SCROZZ-128-042";
    let matrix = encoded_matrix(payload, BarcodeFormat::CODE_128, 480, 120);
    let frame = matrix_frame(&matrix);

    let found = detector(Symbology::Code128)
        .detect(&frame)
        .expect("Code 128 should decode");

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].payload, payload);
    assert_eq!(found[0].symbology, Symbology::Code128);
    assert!(!found[0].bounds.is_empty());
    assert!(found[0].corners.is_empty());
    let (left, top, right, bottom) = ink_bounds(&matrix);
    let bounds = found[0].bounds;
    assert!(bounds.origin.x <= left + 3.0, "{bounds:?}");
    assert!(bounds.origin.y <= top + 3.0, "{bounds:?}");
    assert!(
        bounds.origin.x + bounds.size.width >= right - 3.0,
        "{bounds:?}"
    );
    assert!(
        bounds.origin.y + bounds.size.height >= bottom - 3.0,
        "{bounds:?}"
    );
    assert!(
        bounds.size.height > 0.75 * f64::from(matrix.getHeight()),
        "a scanline is not a symbol bound: {bounds:?}"
    );
}

#[test]
fn decodes_an_inverted_generated_linear_barcode() {
    let payload = "SCROZZ-INVERTED-128";
    let matrix = encoded_matrix(payload, BarcodeFormat::CODE_128, 480, 120);
    let frame = inverted_frame(&matrix);

    let found = detector(Symbology::Code128)
        .detect(&frame)
        .expect("inverted Code 128 should decode");

    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].payload, payload);
    assert_eq!(found[0].symbology, Symbology::Code128);
    assert!(!found[0].bounds.is_empty());
}

#[test]
fn preserves_geometry_for_a_rotated_generated_qr() {
    let payload = "https://example.org/rotated";
    let matrix = encoded_matrix(payload, BarcodeFormat::QR_CODE, 240, 240);
    let rotated = rotated_matrix(&matrix, std::f64::consts::FRAC_PI_4, 24);
    let frame = matrix_frame(&rotated);

    let found = detector(Symbology::QrCode)
        .detect(&frame)
        .expect("rotated QR should decode");

    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].payload, payload);
    assert!(!found[0].bounds.is_empty());
    assert_eq!(found[0].corners.len(), 4, "{found:#?}");
}

#[test]
fn preserves_equal_qr_payloads_at_distinct_positions() {
    let payload = "https://example.org/duplicate";
    let first = encoded_matrix(payload, BarcodeFormat::QR_CODE, 180, 180);
    let second = encoded_matrix(payload, BarcodeFormat::QR_CODE, 180, 180);
    let frame = composite_frame(420, 220, &[(&first, 20, 20), (&second, 220, 20)]);

    let found = detector(Symbology::QrCode)
        .detect(&frame)
        .expect("both QR codes should decode");

    assert_eq!(found.len(), 2, "{found:#?}");
    assert!(found.iter().all(|barcode| barcode.payload == payload));
    assert!(found[0].bounds.origin.x + found[0].bounds.size.width < found[1].bounds.origin.x);
}

#[test]
fn preserves_equal_code_128_payloads_at_distinct_positions() {
    let payload = "SCROZZ-DUPLICATE-128";
    let first = encoded_matrix(payload, BarcodeFormat::CODE_128, 380, 90);
    let second = encoded_matrix(payload, BarcodeFormat::CODE_128, 380, 90);
    let frame = composite_frame(420, 240, &[(&first, 20, 20), (&second, 20, 130)]);

    let found = detector(Symbology::Code128)
        .detect(&frame)
        .expect("both Code 128 symbols should decode");

    assert_eq!(found.len(), 2, "{found:#?}");
    assert!(found.iter().all(|barcode| barcode.payload == payload));
    assert!(found[0].bounds.origin.y + found[0].bounds.size.height < found[1].bounds.origin.y);
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
