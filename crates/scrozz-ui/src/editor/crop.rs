//! Structural boundary preprocessing for content-aware crop snapping.

use scrozz_annotate::AnalysisCancellation;
use scrozz_core::{Error, Frame, LogicalRect, PixelFormat, Result};

/// Maximum number of averaged cells retained by crop analysis.
pub const MAX_BOUNDARY_SAMPLES: u64 = 1_048_576;

const MIN_EDGE_STRENGTH: f32 = 0.065;
const MIN_MEAN_STRENGTH: f32 = 0.095;
const MIN_CONTINUITY: f32 = 0.70;
const MIN_DIRECTIONAL_COHERENCE: f32 = 0.45;

/// Axis perpendicular to a detected boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BoundaryAxis {
    /// A horizontal segment at a fixed `position`, spanning x.
    Horizontal,
    /// A vertical segment at a fixed `position`, spanning y.
    Vertical,
}

/// One long, high-confidence screenshot boundary in source logical points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundarySegment {
    /// Segment direction.
    pub axis: BoundaryAxis,
    /// Y for horizontal segments, x for vertical segments.
    pub position: f64,
    /// Start along the segment's direction.
    pub start: f64,
    /// End along the segment's direction.
    pub end: f64,
    /// Deterministic 0-1 confidence.
    pub confidence: f32,
}

impl BoundarySegment {
    fn distance(self, position: f64) -> f64 {
        (self.position - position).abs()
    }

    pub(crate) fn overlaps(self, start: f64, end: f64) -> bool {
        let overlap = (self.end.min(end) - self.start.max(start)).max(0.0);
        let request = (end - start).max(0.0);
        let segment = (self.end - self.start).max(0.0);
        overlap >= request.min(segment) * 0.35
    }
}

/// Reusable, immutable result of source-image preprocessing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StructuralBoundaryIndex {
    segments: Vec<BoundarySegment>,
}

impl StructuralBoundaryIndex {
    /// Detects long axis-aligned UI structure without retaining source pixels.
    ///
    /// The source is averaged into at most [`MAX_BOUNDARY_SAMPLES`] cells, then
    /// evaluated at three gradient scales. Short, discontinuous, or directionally
    /// incoherent edges are rejected, which filters text glyphs, icons, and noisy
    /// texture in favour of panel seams and table or navigation dividers.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for a malformed frame and
    /// [`Error::Cancelled`] when `cancellation` is raised.
    pub fn analyze(frame: &Frame, cancellation: &AnalysisCancellation) -> Result<Self> {
        if !frame.is_well_formed() {
            return Err(Error::InvalidRequest(
                "crop boundary analysis requires a well-formed frame".to_owned(),
            ));
        }
        let grid = SampleGrid::from_frame(frame, cancellation)?;
        let mut segments = Vec::new();
        detect_vertical(&grid, frame.scale.get(), &mut segments);
        detect_horizontal(&grid, frame.scale.get(), &mut segments);
        deduplicate(&mut segments, f64::from(grid.step) / frame.scale.get());
        segments.sort_by(|left, right| {
            axis_order(left.axis)
                .cmp(&axis_order(right.axis))
                .then_with(|| left.position.total_cmp(&right.position))
                .then_with(|| left.start.total_cmp(&right.start))
        });
        Ok(Self { segments })
    }

    /// Detected boundaries, in stable axis/position order.
    #[must_use]
    pub fn segments(&self) -> &[BoundarySegment] {
        &self.segments
    }

    /// Nearest eligible boundary within `tolerance`.
    #[must_use]
    pub fn nearest(
        &self,
        axis: BoundaryAxis,
        position: f64,
        span: (f64, f64),
        tolerance: f64,
    ) -> Option<BoundarySegment> {
        self.segments
            .iter()
            .copied()
            .filter(|segment| segment.axis == axis)
            .filter(|segment| segment.distance(position) <= tolerance)
            .filter(|segment| segment.overlaps(span.0.min(span.1), span.0.max(span.1)))
            .min_by(|left, right| {
                left.distance(position)
                    .total_cmp(&right.distance(position))
                    .then_with(|| right.confidence.total_cmp(&left.confidence))
            })
    }
}

const fn axis_order(axis: BoundaryAxis) -> u8 {
    match axis {
        BoundaryAxis::Horizontal => 0,
        BoundaryAxis::Vertical => 1,
    }
}

#[derive(Debug)]
struct SampleGrid {
    width: usize,
    height: usize,
    step: u32,
    rgb: Vec<[f32; 3]>,
}

impl SampleGrid {
    fn from_frame(frame: &Frame, cancellation: &AnalysisCancellation) -> Result<Self> {
        let (width, height) = (frame.width(), frame.height());
        let mut step = 1_u32;
        while u64::from(width.div_ceil(step)) * u64::from(height.div_ceil(step))
            > MAX_BOUNDARY_SAMPLES
        {
            step += 1;
        }
        let grid_width = width.div_ceil(step) as usize;
        let grid_height = height.div_ceil(step) as usize;
        let mut rgb = Vec::new();
        rgb.try_reserve_exact(grid_width.saturating_mul(grid_height))
            .map_err(|error| {
                Error::InvalidRequest(format!(
                    "crop boundary sample grid is not allocatable: {error}"
                ))
            })?;

        for grid_y in 0..grid_height {
            if grid_y.is_multiple_of(16) && cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let y0 = grid_y as u32 * step;
            let y1 = (y0 + step).min(height);
            for grid_x in 0..grid_width {
                let x0 = grid_x as u32 * step;
                let x1 = (x0 + step).min(width);
                let mut sum = [0_u64; 3];
                let mut count = 0_u64;
                for y in y0..y1 {
                    for x in x0..x1 {
                        let pixel = read_rgb(frame, x as usize, y as usize);
                        sum[0] += u64::from(pixel[0]);
                        sum[1] += u64::from(pixel[1]);
                        sum[2] += u64::from(pixel[2]);
                        count += 1;
                    }
                }
                let denominator = (count.max(1) * 255) as f32;
                rgb.push([
                    sum[0] as f32 / denominator,
                    sum[1] as f32 / denominator,
                    sum[2] as f32 / denominator,
                ]);
            }
        }
        Ok(Self {
            width: grid_width,
            height: grid_height,
            step,
            rgb,
        })
    }

    fn get(&self, x: usize, y: usize) -> [f32; 3] {
        self.rgb[y * self.width + x]
    }
}

fn read_rgb(frame: &Frame, x: usize, y: usize) -> [u8; 3] {
    let offset = y * frame.stride + x * 4;
    let pixel = &frame.data[offset..offset + 4];
    let (mut r, mut g, mut b) = match frame.format {
        PixelFormat::Rgba8 | PixelFormat::RgbaPremultiplied8 => (pixel[0], pixel[1], pixel[2]),
        PixelFormat::Bgra8 | PixelFormat::BgraPremultiplied8 => (pixel[2], pixel[1], pixel[0]),
    };
    if frame.format.is_premultiplied() && pixel[3] > 0 && pixel[3] < 255 {
        let alpha = u16::from(pixel[3]);
        let straight =
            |channel: u8| ((u16::from(channel) * 255 + alpha / 2) / alpha).min(255) as u8;
        r = straight(r);
        g = straight(g);
        b = straight(b);
    }
    [r, g, b]
}

#[derive(Debug, Clone, Copy, Default)]
struct EdgeSample {
    strength: f32,
    signed_luminance: f32,
}

fn edge(a: [f32; 3], b: [f32; 3]) -> EdgeSample {
    let luminance = |color: [f32; 3]| color[0] * 0.2126 + color[1] * 0.7152 + color[2] * 0.0722;
    let signed_luminance = luminance(b) - luminance(a);
    let color = (a[0] - b[0])
        .abs()
        .max((a[1] - b[1]).abs())
        .max((a[2] - b[2]).abs());
    EdgeSample {
        strength: signed_luminance.abs() * 0.65 + color * 0.35,
        signed_luminance,
    }
}

fn multiscale_edge(grid: &SampleGrid, x: usize, y: usize, horizontal: bool) -> EdgeSample {
    let at = |offset: isize| {
        if horizontal {
            grid.get(
                x,
                (y as isize + offset).clamp(0, grid.height as isize - 1) as usize,
            )
        } else {
            grid.get(
                (x as isize + offset).clamp(0, grid.width as isize - 1) as usize,
                y,
            )
        }
    };
    let fine = edge(at(-1), at(0));
    let medium = edge(at(-2), at(1));
    let wide = edge(at(-4), at(3));
    EdgeSample {
        strength: fine.strength * 0.50 + medium.strength * 0.30 + wide.strength * 0.20,
        signed_luminance: fine.signed_luminance * 0.50
            + medium.signed_luminance * 0.30
            + wide.signed_luminance * 0.20,
    }
}

fn detect_vertical(grid: &SampleGrid, scale: f64, output: &mut Vec<BoundarySegment>) {
    if grid.width < 2 || grid.height < 2 {
        return;
    }
    for x in 1..grid.width {
        let samples: Vec<_> = (0..grid.height)
            .map(|y| multiscale_edge(grid, x, y, false))
            .collect();
        collect_runs(
            &samples,
            grid.height,
            |start, end, confidence| BoundarySegment {
                axis: BoundaryAxis::Vertical,
                position: x as f64 * f64::from(grid.step) / scale,
                start: start as f64 * f64::from(grid.step) / scale,
                end: end as f64 * f64::from(grid.step) / scale,
                confidence,
            },
            output,
        );
    }
}

fn detect_horizontal(grid: &SampleGrid, scale: f64, output: &mut Vec<BoundarySegment>) {
    if grid.width < 2 || grid.height < 2 {
        return;
    }
    for y in 1..grid.height {
        let samples: Vec<_> = (0..grid.width)
            .map(|x| multiscale_edge(grid, x, y, true))
            .collect();
        collect_runs(
            &samples,
            grid.width,
            |start, end, confidence| BoundarySegment {
                axis: BoundaryAxis::Horizontal,
                position: y as f64 * f64::from(grid.step) / scale,
                start: start as f64 * f64::from(grid.step) / scale,
                end: end as f64 * f64::from(grid.step) / scale,
                confidence,
            },
            output,
        );
    }
}

fn collect_runs(
    samples: &[EdgeSample],
    axis_length: usize,
    make: impl Fn(usize, usize, f32) -> BoundarySegment,
    output: &mut Vec<BoundarySegment>,
) {
    let minimum = (axis_length / 8).max(12).min(axis_length);
    let gap = (axis_length / 256).clamp(1, 3);
    let mut start = None;
    let mut last_active = 0;
    let mut active = 0;
    let mut strength = 0.0;
    let mut signed = 0.0_f32;
    let mut absolute_signed = 0.0_f32;

    let finish = |end: usize,
                  start: &mut Option<usize>,
                  active: &mut usize,
                  strength: &mut f32,
                  signed: &mut f32,
                  absolute_signed: &mut f32,
                  output: &mut Vec<BoundarySegment>| {
        let Some(begin) = start.take() else {
            return;
        };
        let length = end.saturating_sub(begin);
        let continuity = *active as f32 / length.max(1) as f32;
        let mean = *strength / (*active).max(1) as f32;
        let coherence = signed.abs() / absolute_signed.max(f32::EPSILON);
        if length >= minimum
            && continuity >= MIN_CONTINUITY
            && mean >= MIN_MEAN_STRENGTH
            && coherence >= MIN_DIRECTIONAL_COHERENCE
        {
            let confidence = (mean * continuity * (0.5 + coherence * 0.5)).clamp(0.0, 1.0);
            output.push(make(begin, end, confidence));
        }
        *active = 0;
        *strength = 0.0;
        *signed = 0.0;
        *absolute_signed = 0.0;
    };

    for (index, sample) in samples.iter().copied().enumerate() {
        if sample.strength >= MIN_EDGE_STRENGTH {
            start.get_or_insert(index);
            last_active = index;
            active += 1;
            strength += sample.strength;
            signed += sample.signed_luminance;
            absolute_signed += sample.signed_luminance.abs();
        } else if start.is_some() && index.saturating_sub(last_active) > gap {
            finish(
                last_active + 1,
                &mut start,
                &mut active,
                &mut strength,
                &mut signed,
                &mut absolute_signed,
                output,
            );
        }
    }
    finish(
        last_active + usize::from(start.is_some()),
        &mut start,
        &mut active,
        &mut strength,
        &mut signed,
        &mut absolute_signed,
        output,
    );
}

fn deduplicate(segments: &mut Vec<BoundarySegment>, reach: f64) {
    segments.sort_by(|left, right| {
        axis_order(left.axis)
            .cmp(&axis_order(right.axis))
            .then_with(|| left.position.total_cmp(&right.position))
            .then_with(|| right.confidence.total_cmp(&left.confidence))
    });
    let mut kept: Vec<BoundarySegment> = Vec::with_capacity(segments.len());
    for segment in segments.drain(..) {
        let duplicate = kept.iter().position(|existing| {
            existing.axis == segment.axis
                && (existing.position - segment.position).abs() <= reach * 8.0
                && existing.overlaps(segment.start, segment.end)
        });
        match duplicate {
            Some(index) if segment.confidence > kept[index].confidence => kept[index] = segment,
            Some(_) => {}
            None => kept.push(segment),
        }
    }
    *segments = kept;
}

/// Display-space span of a source rectangle for a given boundary axis.
#[must_use]
pub fn source_span(rect: LogicalRect, axis: BoundaryAxis) -> (f64, f64) {
    match axis {
        BoundaryAxis::Horizontal => (rect.origin.x, rect.origin.x + rect.size.width),
        BoundaryAxis::Vertical => (rect.origin.y, rect.origin.y + rect.size.height),
    }
}

#[cfg(test)]
mod tests {
    use scrozz_annotate::AnalysisCancellation;
    use scrozz_core::{ColorSpace, Error, Frame, PhysicalSize, PixelFormat, ScaleFactor};

    use super::{BoundaryAxis, StructuralBoundaryIndex};

    fn frame(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Frame {
        let mut data = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                data.extend_from_slice(&pixel(x, y));
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

    #[test]
    fn detects_a_long_panel_seam_as_one_precise_segment() {
        let frame = frame(256, 160, |x, _| {
            if x < 128 {
                [32, 40, 48, 255]
            } else {
                [210, 218, 226, 255]
            }
        });
        let index =
            StructuralBoundaryIndex::analyze(&frame, &AnalysisCancellation::default()).unwrap();

        let seam = index
            .nearest(BoundaryAxis::Vertical, 128.0, (20.0, 140.0), 2.0)
            .expect("long, coherent panel seam");
        assert!((seam.position - 128.0).abs() <= 1.0);
        assert!(seam.start <= 2.0);
        assert!(seam.end >= 158.0);
    }

    #[test]
    fn rejects_small_icons_low_contrast_edges_and_noisy_texture() {
        let icon = frame(256, 160, |x, y| {
            if (20..28).contains(&x) && (20..28).contains(&y) {
                [20, 20, 20, 255]
            } else if x >= 128 {
                [205, 205, 205, 255]
            } else {
                [200, 200, 200, 255]
            }
        });
        let icon_index =
            StructuralBoundaryIndex::analyze(&icon, &AnalysisCancellation::default()).unwrap();
        assert!(icon_index.segments().is_empty());

        let noise = frame(256, 160, |x, y| {
            if (x + y).is_multiple_of(2) {
                [0, 0, 0, 255]
            } else {
                [255, 255, 255, 255]
            }
        });
        let noise_index =
            StructuralBoundaryIndex::analyze(&noise, &AnalysisCancellation::default()).unwrap();
        assert!(
            noise_index.segments().is_empty(),
            "alternating texture must not look like coherent screenshot structure"
        );
    }

    #[test]
    fn cancellation_is_observed_before_preprocessing_work() {
        let cancellation = AnalysisCancellation::default();
        cancellation.cancel();
        let result =
            StructuralBoundaryIndex::analyze(&frame(64, 64, |_, _| [0, 0, 0, 255]), &cancellation);
        assert!(matches!(result, Err(Error::Cancelled)));
    }
}
