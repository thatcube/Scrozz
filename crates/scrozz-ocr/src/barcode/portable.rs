//! Pure-Rust barcode detection used on Windows and Linux and as the macOS fallback.

use std::collections::HashSet;

use rxing::common::{BitMatrix, HybridBinarizer};
use rxing::{
    BarcodeFormat, Binarizer, BinaryBitmap, DecodeHints, Exceptions, Luma8LuminanceSource,
    MultiFormatReader, Point, RXingResult, Reader,
};
use scrozz_core::{Error, Frame, LogicalRect, Result};

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

        // RXing's AlsoInverted path does not invert rows consumed by every 1-D
        // reader, so run both luminance polarities explicitly. This also permits
        // normal and inverted symbols to coexist in one image.
        let inverted_luma = luma.iter().map(|sample| 255 - sample).collect();
        let mut first_error = None;
        let mut barcodes = Vec::new();
        for pixels in [luma, inverted_luma] {
            match decode_pass(pixels, prepared.image.width, prepared.image.height, &hints) {
                Ok(Some(pass)) => {
                    barcodes.extend(pass.decoded.iter().filter_map(|result| {
                        decoded_barcode(result, &pass.matrix, &prepared, frame, &self.options)
                    }));
                }
                Ok(None) => {}
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if barcodes.is_empty()
            && let Some(error) = first_error
        {
            return Err(portable_error(error));
        }

        let mut barcodes = deduplicate_overlaps(barcodes);
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

const MULTI_MIN_REMAINDER: f32 = 64.0;
const MULTI_MAX_DEPTH: u8 = 4;

struct DecodePass {
    decoded: Vec<RXingResult>,
    matrix: BitMatrix,
}

fn decode_pass(
    luma: Vec<u8>,
    width: u32,
    height: u32,
    hints: &DecodeHints,
) -> std::result::Result<Option<DecodePass>, Exceptions> {
    let source = Luma8LuminanceSource::new(luma, width, height)?;
    let mut bitmap = BinaryBitmap::new(HybridBinarizer::new(source));
    let matrix = bitmap.get_black_matrix().clone();
    let mut reader = MultiFormatReader::default();
    match decode_spatially(&mut reader, &mut bitmap, hints) {
        Ok(decoded) => Ok(Some(DecodePass { decoded, matrix })),
        Err(Exceptions::NotFoundException(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn decode_spatially<B: Binarizer>(
    reader: &mut MultiFormatReader,
    bitmap: &mut BinaryBitmap<B>,
    hints: &DecodeHints,
) -> std::result::Result<Vec<RXingResult>, Exceptions> {
    let mut decoded = Vec::new();
    decode_region(reader, bitmap, hints, &mut decoded, (0.0, 0.0), 0, true)?;
    if decoded.is_empty() {
        Err(Exceptions::NOT_FOUND)
    } else {
        Ok(decoded)
    }
}

fn decode_region<B: Binarizer>(
    reader: &mut MultiFormatReader,
    bitmap: &mut BinaryBitmap<B>,
    hints: &DecodeHints,
    decoded: &mut Vec<RXingResult>,
    offset: (f32, f32),
    depth: u8,
    root: bool,
) -> std::result::Result<(), Exceptions> {
    if depth > MULTI_MAX_DEPTH {
        return Ok(());
    }

    let mut result = match reader.decode_with_hints(bitmap, hints) {
        Ok(result) => result,
        Err(Exceptions::NotFoundException(_)) => return Ok(()),
        // Crops intentionally cut through the already-found symbol. A partial
        // crop may therefore look malformed and is not a whole-image failure.
        Err(_) if !root => return Ok(()),
        Err(error) => return Err(error),
    };
    let local_points = result
        .getPoints()
        .iter()
        .copied()
        .filter(|point| point.x.is_finite() && point.y.is_finite())
        .collect::<Vec<_>>();
    for point in result.getPointsMut() {
        point.x += offset.0;
        point.y += offset.1;
    }
    decoded.push(result);

    if local_points.is_empty() {
        return Ok(());
    }

    let width = bitmap.get_width();
    let height = bitmap.get_height();
    let min_x = local_points
        .iter()
        .map(|point| point.x)
        .fold(width as f32, f32::min)
        .clamp(0.0, width as f32);
    let min_y = local_points
        .iter()
        .map(|point| point.y)
        .fold(height as f32, f32::min)
        .clamp(0.0, height as f32);
    let max_x = local_points
        .iter()
        .map(|point| point.x)
        .fold(0.0, f32::max)
        .clamp(0.0, width as f32);
    let max_y = local_points
        .iter()
        .map(|point| point.y)
        .fold(0.0, f32::max)
        .clamp(0.0, height as f32);

    let left_width = min_x.floor() as usize;
    if min_x > MULTI_MIN_REMAINDER && left_width > 0 {
        decode_region(
            reader,
            &mut bitmap.crop(0, 0, left_width, height),
            hints,
            decoded,
            offset,
            depth + 1,
            false,
        )?;
    }

    let above_height = min_y.floor() as usize;
    if min_y > MULTI_MIN_REMAINDER && above_height > 0 {
        decode_region(
            reader,
            &mut bitmap.crop(0, 0, width, above_height),
            hints,
            decoded,
            offset,
            depth + 1,
            false,
        )?;
    }

    let right_start = max_x.floor() as usize;
    if width.saturating_sub(right_start) as f32 > MULTI_MIN_REMAINDER {
        decode_region(
            reader,
            &mut bitmap.crop(right_start, 0, width - right_start, height),
            hints,
            decoded,
            (offset.0 + right_start as f32, offset.1),
            depth + 1,
            false,
        )?;
    }

    let below_start = max_y.floor() as usize;
    if height.saturating_sub(below_start) as f32 > MULTI_MIN_REMAINDER {
        decode_region(
            reader,
            &mut bitmap.crop(0, below_start, width, height - below_start),
            hints,
            decoded,
            (offset.0, offset.1 + below_start as f32),
            depth + 1,
            false,
        )?;
    }
    Ok(())
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
    matrix: &BitMatrix,
    prepared: &Prepared,
    frame: &Frame,
    options: &BarcodeOptions,
) -> Option<Barcode> {
    let symbology = symbology(*result.getBarcodeFormat());
    if !options.accepts(&symbology) {
        return None;
    }

    let geometry = barcode_geometry(result, &symbology, matrix);
    let bounds = geometry
        .as_ref()
        .map_or_else(LogicalRect::default, |geometry| {
            prepared_pixels_to_logical(
                f64::from(geometry.bounds.left),
                f64::from(geometry.bounds.top),
                f64::from(geometry.bounds.width()),
                f64::from(geometry.bounds.height()),
                prepared,
                frame,
            )
        });
    let corners = geometry.map_or_else(Vec::new, |geometry| {
        geometry
            .corners
            .iter()
            .map(|point| {
                prepared_point_to_logical(f64::from(point.x), f64::from(point.y), prepared, frame)
            })
            .collect()
    });

    Some(Barcode {
        payload: result.getText().to_owned(),
        symbology,
        bounds,
        corners,
        confidence: 1.0,
    })
}

#[derive(Debug, Clone, Copy)]
struct PixelBounds {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl PixelBounds {
    fn from_points(points: &[Point]) -> Option<Self> {
        let first = *points.first()?;
        let bounds = points.iter().skip(1).fold(
            Self {
                left: first.x,
                top: first.y,
                right: first.x,
                bottom: first.y,
            },
            |bounds, point| Self {
                left: bounds.left.min(point.x),
                top: bounds.top.min(point.y),
                right: bounds.right.max(point.x),
                bottom: bounds.bottom.max(point.y),
            },
        );
        (bounds.left.is_finite()
            && bounds.top.is_finite()
            && bounds.right.is_finite()
            && bounds.bottom.is_finite()
            && bounds.width() > 0.0
            && bounds.height() > 0.0)
            .then_some(bounds)
    }

    fn width(self) -> f32 {
        self.right - self.left
    }

    fn height(self) -> f32 {
        self.bottom - self.top
    }
}

#[derive(Debug)]
struct PixelGeometry {
    bounds: PixelBounds,
    corners: Vec<Point>,
}

fn barcode_geometry(
    result: &RXingResult,
    symbology: &Symbology,
    matrix: &BitMatrix,
) -> Option<PixelGeometry> {
    let points = result
        .getPoints()
        .iter()
        .copied()
        .filter(|point| point.x.is_finite() && point.y.is_finite())
        .collect::<Vec<_>>();
    if points.is_empty() {
        return None;
    }

    match symbology {
        Symbology::QrCode => qr_geometry(&points, matrix),
        symbology if symbology.is_matrix() => matrix_ink_geometry(&points, matrix),
        _ => linear_geometry(&points, matrix),
    }
}

fn qr_geometry(points: &[Point], matrix: &BitMatrix) -> Option<PixelGeometry> {
    // RXing's C++-ported reader instead returns a symbol-position
    // quadrilateral in [top-left, top-right, bottom-left, bottom-right] order.
    // The classic reader returns finder/alignment landmarks in a different
    // topology. Never extrapolate those landmarks into claimed corners.
    if cpp_qr_outline_layout(points)
        && let Some(geometry) = validated_qr_outline(points, matrix)
    {
        return Some(geometry);
    }
    // Unproven points retain only a conservative ink bound.
    matrix_ink_geometry(points, matrix)
}

fn cpp_qr_outline_layout(points: &[Point]) -> bool {
    if points.len() != 4 {
        return false;
    }
    let horizontal = distance(points[0], points[1]);
    let vertical = distance(points[0], points[2]);
    if horizontal < 2.0 || vertical < 2.0 {
        return false;
    }

    // Perspective can move the fourth corner away from the affine estimate, but
    // it remains far closer than a classic alignment-pattern center interpreted
    // under the C++ reader's ordering.
    let affine_bottom_right = subtract(add(points[1], points[2]), points[0]);
    distance(affine_bottom_right, points[3]) <= horizontal.max(vertical) * 0.65
}

fn validated_qr_outline(points: &[Point], matrix: &BitMatrix) -> Option<PixelGeometry> {
    let corners = canonical_quadrilateral(points)?;
    let geometry = PixelGeometry {
        bounds: PixelBounds::from_points(&corners)?,
        corners,
    };
    qr_outline_matches_ink(&geometry, points, matrix).then_some(geometry)
}

fn qr_outline_matches_ink(geometry: &PixelGeometry, anchors: &[Point], matrix: &BitMatrix) -> bool {
    let Some(ink) = local_ink_points(anchors, matrix) else {
        return false;
    };
    let Some(ink_bounds) = PixelBounds::from_points(&ink) else {
        return false;
    };
    let tolerance = ink_bounds
        .width()
        .max(ink_bounds.height())
        .mul_add(0.01, 1.0)
        .max(2.0);
    (geometry.bounds.left - (ink_bounds.left - 0.5)).abs() <= tolerance
        && (geometry.bounds.top - (ink_bounds.top - 0.5)).abs() <= tolerance
        && (geometry.bounds.right - (ink_bounds.right + 0.5)).abs() <= tolerance
        && (geometry.bounds.bottom - (ink_bounds.bottom + 0.5)).abs() <= tolerance
        && ink
            .iter()
            .all(|point| point_in_convex_outline(*point, &geometry.corners, 1.5))
}

fn point_in_convex_outline(point: Point, corners: &[Point], tolerance: f32) -> bool {
    let mut positive = false;
    let mut negative = false;
    for index in 0..corners.len() {
        let start = corners[index];
        let end = corners[(index + 1) % corners.len()];
        let edge = subtract(end, start);
        let relative = subtract(point, start);
        let signed = edge.x * relative.y - edge.y * relative.x;
        let allowance = tolerance * distance(start, end);
        positive |= signed > allowance;
        negative |= signed < -allowance;
        if positive && negative {
            return false;
        }
    }
    true
}

fn canonical_quadrilateral(points: &[Point]) -> Option<Vec<Point>> {
    if points.len() != 4 {
        return None;
    }
    for (index, point) in points.iter().enumerate() {
        if points[index + 1..]
            .iter()
            .any(|other| distance(*point, *other) < 0.5)
        {
            return None;
        }
    }

    let centroid = Point::new(
        points.iter().map(|point| point.x).sum::<f32>() / 4.0,
        points.iter().map(|point| point.y).sum::<f32>() / 4.0,
    );
    let mut ordered = points.to_vec();
    ordered.sort_by(|left, right| {
        (left.y - centroid.y)
            .atan2(left.x - centroid.x)
            .total_cmp(&(right.y - centroid.y).atan2(right.x - centroid.x))
    });

    let mut orientation = None;
    for index in 0..4 {
        let turn = cross(
            ordered[index],
            ordered[(index + 1) % 4],
            ordered[(index + 2) % 4],
        );
        if turn.abs() < 0.5 {
            return None;
        }
        let sign = turn.is_sign_positive();
        if orientation.is_some_and(|orientation| orientation != sign) {
            return None;
        }
        orientation = Some(sign);
    }
    if orientation == Some(false) {
        ordered.reverse();
    }

    let first = ordered
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            left.y
                .total_cmp(&right.y)
                .then_with(|| left.x.total_cmp(&right.x))
        })
        .map(|(index, _)| index)?;
    ordered.rotate_left(first);
    Some(ordered)
}

fn cross(origin: Point, first: Point, second: Point) -> f32 {
    let one = subtract(first, origin);
    let two = subtract(second, first);
    one.x * two.y - one.y * two.x
}

fn matrix_ink_geometry(points: &[Point], matrix: &BitMatrix) -> Option<PixelGeometry> {
    let ink = local_ink_points(points, matrix)?;
    let bounds = PixelBounds::from_points(&ink)?;
    (bounds.width() >= 2.0 && bounds.height() >= 2.0).then_some(PixelGeometry {
        bounds: PixelBounds {
            left: bounds.left - 0.5,
            top: bounds.top - 0.5,
            right: bounds.right + 0.5,
            bottom: bounds.bottom + 0.5,
        },
        corners: Vec::new(),
    })
}

fn local_ink_points(points: &[Point], matrix: &BitMatrix) -> Option<Vec<Point>> {
    let anchors = PixelBounds::from_points(points)?;
    let margin = anchors.width().max(anchors.height()).mul_add(0.3, 8.0);
    let left = (anchors.left - margin).floor().max(0.0) as u32;
    let top = (anchors.top - margin).floor().max(0.0) as u32;
    let right = (anchors.right + margin)
        .ceil()
        .min(matrix.getWidth() as f32) as u32;
    let bottom = (anchors.bottom + margin)
        .ceil()
        .min(matrix.getHeight() as f32) as u32;
    if right <= left || bottom <= top {
        return None;
    }

    let mut border_on = 0usize;
    let mut border_total = 0usize;
    for x in left..right {
        border_on += usize::from(matrix.get(x, top));
        border_on += usize::from(matrix.get(x, bottom - 1));
        border_total += 2;
    }
    for y in top.saturating_add(1)..bottom.saturating_sub(1) {
        border_on += usize::from(matrix.get(left, y));
        border_on += usize::from(matrix.get(right - 1, y));
        border_total += 2;
    }
    let foreground = border_on * 2 < border_total;

    let mut ink = Vec::new();
    for y in top..bottom {
        for x in left..right {
            if matrix.get(x, y) == foreground {
                ink.push(Point::new(x as f32, y as f32));
            }
        }
    }
    Some(ink)
}

fn linear_geometry(points: &[Point], matrix: &BitMatrix) -> Option<PixelGeometry> {
    if points.len() < 2 {
        return None;
    }
    let start_anchor = points[0];
    let end_anchor = points[1];
    let anchor_distance = distance(start_anchor, end_anchor);
    if !anchor_distance.is_finite() || anchor_distance < 2.0 {
        return None;
    }
    let axis = scale(subtract(end_anchor, start_anchor), 1.0 / anchor_distance);
    let runs = line_runs(matrix, start_anchor, axis)?;
    let start_run = closest_run(&runs, 0.0)?;
    let end_run = closest_run(&runs, anchor_distance)?;
    let (inside_start, inside_end) = if start_run <= end_run {
        (start_run, end_run)
    } else {
        (end_run, start_run)
    };
    let module = narrow_module(&runs[inside_start..=inside_end]);
    let quiet_run = (module * 6.0).ceil().max(6.0);

    let left_quiet = runs[..inside_start]
        .iter()
        .enumerate()
        .rev()
        .find(|(_, run)| run.length() >= quiet_run)
        .map(|(index, _)| index);
    let right_quiet = runs[inside_end.saturating_add(1)..]
        .iter()
        .enumerate()
        .find(|(_, run)| run.length() >= quiet_run)
        .map(|(index, _)| index + inside_end + 1);
    let axis_start = left_quiet.map_or(0.0, |index| runs[index].end + 0.5);
    let axis_end = right_quiet.map_or(anchor_distance, |index| runs[index].start - 0.5);
    if axis_end - axis_start < 2.0 {
        return None;
    }

    let line_start = add(start_anchor, scale(axis, axis_start));
    let line_end = add(start_anchor, scale(axis, axis_end));
    let perpendicular = Point::new(-axis.y, axis.x);
    let baseline = line_stats(matrix, line_start, line_end);
    if baseline.transitions < 6 {
        return None;
    }
    let transition_floor = (baseline.transitions * 3 / 5).max(4);
    let before = barcode_extent(
        matrix,
        line_start,
        line_end,
        scale(perpendicular, -1.0),
        transition_floor,
    );
    let after = barcode_extent(
        matrix,
        line_start,
        line_end,
        perpendicular,
        transition_floor,
    );
    if before + after + 1.0 < 3.0 {
        return None;
    }

    let corners = [
        add(line_start, scale(perpendicular, -before - 0.5)),
        add(line_end, scale(perpendicular, -before - 0.5)),
        add(line_end, scale(perpendicular, after + 0.5)),
        add(line_start, scale(perpendicular, after + 0.5)),
    ];
    Some(PixelGeometry {
        bounds: PixelBounds::from_points(&corners)?,
        // A linear decoder finds a scan line, not four measured corners.
        corners: Vec::new(),
    })
}

#[derive(Debug, Clone, Copy)]
struct BinaryRun {
    start: f32,
    end: f32,
}

impl BinaryRun {
    fn length(self) -> f32 {
        self.end - self.start + 1.0
    }
}

fn line_runs(matrix: &BitMatrix, origin: Point, axis: Point) -> Option<Vec<BinaryRun>> {
    let (minimum, maximum) = line_parameter_bounds(matrix, origin, axis)?;
    let first = minimum.ceil() as i32;
    let last = maximum.floor() as i32;
    if last <= first {
        return None;
    }

    let mut runs = Vec::new();
    let mut run_start = first;
    let mut previous = sample(matrix, add(origin, scale(axis, first as f32)))?;
    for parameter in first.saturating_add(1)..=last {
        let value = sample(matrix, add(origin, scale(axis, parameter as f32)))?;
        if value != previous {
            runs.push(BinaryRun {
                start: run_start as f32,
                end: (parameter - 1) as f32,
            });
            run_start = parameter;
            previous = value;
        }
    }
    runs.push(BinaryRun {
        start: run_start as f32,
        end: last as f32,
    });
    Some(runs)
}

fn line_parameter_bounds(
    matrix: &BitMatrix,
    origin: Point,
    direction: Point,
) -> Option<(f32, f32)> {
    let mut minimum = f32::NEG_INFINITY;
    let mut maximum = f32::INFINITY;
    for (coordinate, delta, limit) in [
        (origin.x, direction.x, matrix.getWidth() as f32 - 1.0),
        (origin.y, direction.y, matrix.getHeight() as f32 - 1.0),
    ] {
        if delta.abs() < f32::EPSILON {
            if coordinate < 0.0 || coordinate > limit {
                return None;
            }
            continue;
        }
        let one = -coordinate / delta;
        let two = (limit - coordinate) / delta;
        minimum = minimum.max(one.min(two));
        maximum = maximum.min(one.max(two));
    }
    (minimum.is_finite() && maximum.is_finite() && maximum >= minimum).then_some((minimum, maximum))
}

fn closest_run(runs: &[BinaryRun], parameter: f32) -> Option<usize> {
    runs.iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            run_distance(**left, parameter).total_cmp(&run_distance(**right, parameter))
        })
        .map(|(index, _)| index)
}

fn run_distance(run: BinaryRun, parameter: f32) -> f32 {
    if parameter < run.start {
        run.start - parameter
    } else if parameter > run.end {
        parameter - run.end
    } else {
        0.0
    }
}

fn narrow_module(runs: &[BinaryRun]) -> f32 {
    let mut lengths = runs
        .iter()
        .skip(1)
        .take(runs.len().saturating_sub(2))
        .map(|run| run.length())
        .filter(|length| *length >= 1.0)
        .collect::<Vec<_>>();
    if lengths.is_empty() {
        return 1.0;
    }
    lengths.sort_by(f32::total_cmp);
    lengths[lengths.len() / 5]
}

#[derive(Debug, Clone, Copy)]
struct LineStats {
    samples: usize,
    transitions: usize,
}

fn line_stats(matrix: &BitMatrix, start: Point, end: Point) -> LineStats {
    let length = distance(start, end);
    let steps = length.ceil().max(1.0) as usize;
    let mut previous = None;
    let mut transitions = 0;
    let mut samples = 0;
    for step in 0..=steps {
        let ratio = step as f32 / steps as f32;
        let point = add(start, scale(subtract(end, start), ratio));
        let Some(value) = sample(matrix, point) else {
            continue;
        };
        if previous.is_some_and(|previous| previous != value) {
            transitions += 1;
        }
        previous = Some(value);
        samples += 1;
    }
    LineStats {
        samples,
        transitions,
    }
}

fn barcode_extent(
    matrix: &BitMatrix,
    start: Point,
    end: Point,
    direction: Point,
    transition_floor: usize,
) -> f32 {
    let limit = matrix.getWidth().max(matrix.getHeight());
    let baseline_samples = line_stats(matrix, start, end).samples;
    let mut last_matching = 0.0;
    let mut misses = 0u8;
    for offset in 1..=limit {
        let delta = scale(direction, offset as f32);
        let stats = line_stats(matrix, add(start, delta), add(end, delta));
        let enough_samples = stats.samples * 4 >= baseline_samples * 3;
        if enough_samples && stats.transitions >= transition_floor {
            last_matching = offset as f32;
            misses = 0;
        } else {
            misses += 1;
            if misses >= 2 {
                break;
            }
        }
    }
    last_matching
}

fn sample(matrix: &BitMatrix, point: Point) -> Option<bool> {
    let x = point.x.round() as i32;
    let y = point.y.round() as i32;
    (x >= 0 && y >= 0 && x < matrix.getWidth() as i32 && y < matrix.getHeight() as i32)
        .then(|| matrix.get(x as u32, y as u32))
}

fn distance(left: Point, right: Point) -> f32 {
    (right.x - left.x).hypot(right.y - left.y)
}

fn add(left: Point, right: Point) -> Point {
    Point::new(left.x + right.x, left.y + right.y)
}

fn subtract(left: Point, right: Point) -> Point {
    Point::new(left.x - right.x, left.y - right.y)
}

fn scale(point: Point, factor: f32) -> Point {
    Point::new(point.x * factor, point.y * factor)
}

fn deduplicate_overlaps(barcodes: Vec<Barcode>) -> Vec<Barcode> {
    let mut unique: Vec<Barcode> = Vec::new();
    for barcode in barcodes {
        let duplicate = unique.iter().any(|existing| {
            existing.payload == barcode.payload
                && existing.symbology == barcode.symbology
                && substantially_overlaps(existing.bounds, barcode.bounds)
        });
        if !duplicate {
            unique.push(barcode);
        }
    }
    unique
}

fn substantially_overlaps(left: LogicalRect, right: LogicalRect) -> bool {
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let overlap_width = (left.origin.x + left.size.width).min(right.origin.x + right.size.width)
        - left.origin.x.max(right.origin.x);
    let overlap_height = (left.origin.y + left.size.height).min(right.origin.y + right.size.height)
        - left.origin.y.max(right.origin.y);
    if overlap_width <= 0.0 || overlap_height <= 0.0 {
        return false;
    }
    let overlap = overlap_width * overlap_height;
    let smaller = (left.size.width * left.size.height).min(right.size.width * right.size.height);
    overlap >= smaller * 0.5
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
    use scrozz_core::{LogicalPoint, PhysicalSize, PixelFormat, ScaleFactor};

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
    fn duplicate_payloads_are_only_collapsed_when_their_bounds_overlap() {
        let barcode = |x| Barcode {
            payload: "same".to_string(),
            symbology: Symbology::Code128,
            bounds: LogicalRect::new(
                LogicalPoint::new(x, 10.0),
                scrozz_core::LogicalSize::new(40.0, 20.0),
            ),
            corners: Vec::new(),
            confidence: 1.0,
        };
        assert_eq!(
            deduplicate_overlaps(vec![barcode(10.0), barcode(12.0)]).len(),
            1
        );
        assert_eq!(
            deduplicate_overlaps(vec![barcode(10.0), barcode(100.0)]).len(),
            2
        );
    }

    #[test]
    fn orders_a_rotated_quadrilateral_cyclically() {
        let points = [
            Point::new(100.0, 170.0),
            Point::new(170.0, 100.0),
            Point::new(100.0, 30.0),
            Point::new(30.0, 100.0),
        ];

        let ordered = canonical_quadrilateral(&points).expect("valid quadrilateral");

        assert_eq!(ordered[0], Point::new(100.0, 30.0));
        assert_eq!(ordered[1], Point::new(170.0, 100.0));
        assert_eq!(ordered[2], Point::new(100.0, 170.0));
        assert_eq!(ordered[3], Point::new(30.0, 100.0));
    }

    #[test]
    fn classic_qr_landmarks_never_become_corners() {
        let mut matrix = BitMatrix::new(200, 200).expect("matrix");
        matrix
            .setRegion(20, 20, 160, 160)
            .expect("generated ink fixture");
        let landmarks = [
            Point::new(20.0, 179.0),
            Point::new(20.0, 20.0),
            Point::new(179.0, 20.0),
            Point::new(150.0, 150.0),
        ];

        let geometry = qr_geometry(&landmarks, &matrix).expect("conservative QR geometry");

        assert!(geometry.corners.is_empty());
        assert!(geometry.bounds.left <= 20.0);
        assert!(geometry.bounds.top <= 20.0);
        assert!(geometry.bounds.right >= 179.0);
        assert!(geometry.bounds.bottom >= 179.0);
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
