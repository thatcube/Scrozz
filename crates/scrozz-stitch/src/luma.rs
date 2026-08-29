//! Turning captured frames into something alignment can be computed on.
//!
//! Alignment does not need colour, and it must not need floating point: a
//! stitched image whose seams land in a different place on ARM than on x86
//! would make D25's golden fixtures flaky, and a flaky golden test is a deleted
//! golden test. Everything here is integer arithmetic on 8-bit luma, so the
//! answer is bit-identical everywhere.
//!
//! The awkward part is that a [`Frame`] is not a rectangle of pixels. It has a
//! stride that usually exceeds its width, one of four channel orders, and its
//! colour may or may not be premultiplied by alpha. All four combinations reach
//! this crate, because the no-eager-conversion rule in [`scrozz_core::frame`]
//! means the capture backends hand their native layout straight through.

use scrozz_core::{Error, Frame, PixelFormat, Result};

/// A tightly packed 8-bit greyscale plane.
///
/// Rows are exactly `width` bytes, which is what lets alignment index by
/// `y * width + x` without carrying a stride through every loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LumaPlane {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl LumaPlane {
    /// Builds a plane from raw samples.
    ///
    /// # Panics
    ///
    /// Panics if `data` is not exactly `width * height` bytes. Only tests
    /// construct planes directly; every other path goes through [`Self::from_frame`].
    #[must_use]
    pub fn from_raw(width: u32, height: u32, data: Vec<u8>) -> Self {
        assert_eq!(
            data.len(),
            width as usize * height as usize,
            "a luma plane is tightly packed"
        );
        Self {
            width,
            height,
            data,
        }
    }

    /// Extracts luminance from a captured frame.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the frame's buffer is too short for
    /// its declared geometry, or if it has no area. Both are caller bugs that
    /// are far cheaper to catch here than three seconds later inside a stitch.
    pub fn from_frame(frame: &Frame) -> Result<Self> {
        let (width, height) = (frame.width(), frame.height());
        if width == 0 || height == 0 {
            return Err(Error::InvalidRequest(format!(
                "a frame for scrolling capture must have area, got {width}×{height}"
            )));
        }
        if !frame.is_well_formed() {
            return Err(Error::InvalidRequest(format!(
                "frame buffer is {} bytes, too short for {width}×{height} at stride {}",
                frame.data.len(),
                frame.stride
            )));
        }

        let bpp = frame.format.bytes_per_pixel();
        // Channel offsets, so the inner loop has no per-pixel `match`.
        let (r_at, b_at) = match frame.format {
            PixelFormat::Rgba8 | PixelFormat::RgbaPremultiplied8 => (0, 2),
            PixelFormat::Bgra8 | PixelFormat::BgraPremultiplied8 => (2, 0),
        };
        let premultiplied = frame.format.is_premultiplied();

        let mut data = vec![0u8; width as usize * height as usize];
        for y in 0..height as usize {
            let row = &frame.data[y * frame.stride..];
            let out = &mut data[y * width as usize..(y + 1) * width as usize];
            for (x, slot) in out.iter_mut().enumerate() {
                let px = &row[x * bpp..x * bpp + bpp];
                let (mut r, mut g, mut b) = (px[r_at], px[1], px[b_at]);
                if premultiplied {
                    let a = px[3];
                    // Recovering straight colour matters here for the same reason
                    // it matters when compositing: a 50%-alpha window edge stored
                    // premultiplied reads as half as bright as it looks, which
                    // moves the correlation peak on frames whose only strong
                    // feature is a translucent sidebar.
                    r = unpremultiply(r, a);
                    g = unpremultiply(g, a);
                    b = unpremultiply(b, a);
                }
                *slot = luma(r, g, b);
            }
        }

        Ok(Self {
            width,
            height,
            data,
        })
    }

    /// Width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// One row of samples.
    ///
    /// # Panics
    ///
    /// Panics if `y` is outside the plane.
    #[must_use]
    pub fn row(&self, y: u32) -> &[u8] {
        let start = y as usize * self.width as usize;
        &self.data[start..start + self.width as usize]
    }

    /// Whether two planes can be compared at all.
    #[must_use]
    pub const fn matches_width(&self, other: &Self) -> bool {
        self.width == other.width
    }

    /// Whether two planes have identical pixel geometry.
    #[must_use]
    pub const fn matches_dimensions(&self, other: &Self) -> bool {
        self.width == other.width && self.height == other.height
    }
}

/// ITU-R BT.601 luma, in integers so the result is identical on every target.
const fn luma(r: u8, g: u8, b: u8) -> u8 {
    // 77/150/29 are the 8-bit fixed-point form of 0.299/0.587/0.114 and sum to
    // 256, so the shift cannot overflow or drift.
    (((r as u32 * 77) + (g as u32 * 150) + (b as u32 * 29)) >> 8) as u8
}

/// Recovers a straight colour channel from a premultiplied one.
const fn unpremultiply(channel: u8, alpha: u8) -> u8 {
    if alpha == 0 {
        // Fully transparent: the colour carries no information, and dividing by
        // zero would be the least of the problems. Black is the neutral choice
        // and, more importantly, the *same* neutral choice in both frames.
        return 0;
    }
    if alpha == 255 {
        return channel;
    }
    let value = (channel as u32 * 255) / alpha as u32;
    if value > 255 { 255 } else { value as u8 }
}

/// A per-row summary used to search the offset space cheaply.
///
/// Comparing every candidate offset pixel by pixel is O(rows × width × range),
/// which for a 1600×1000 capture is hundreds of millions of byte comparisons per
/// frame — enough to make a ten-frame scrolling capture take seconds of pure
/// arithmetic. Reducing each row to a handful of column means first cuts that by
/// the width-to-bucket ratio, and the shortlist it produces is then verified
/// against real pixels, so nothing is decided on the summary alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowProfile {
    buckets: usize,
    height: u32,
    /// `height * buckets` mean values, row-major.
    data: Vec<u16>,
    /// Exact per-bucket sums used to prove lower bounds before full-pixel work.
    sums: Vec<u64>,
}

/// A per-column summary used to search horizontal offsets cheaply.
///
/// This is the transpose of [`RowProfile`]: each column is reduced to a small
/// set of row means, then shortlisted offsets are verified against the original
/// pixels by the aligner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnProfile {
    buckets: usize,
    width: u32,
    /// `width * buckets` mean values, column-major.
    data: Vec<u16>,
    /// Exact per-bucket sums used to prove lower bounds before full-pixel work.
    sums: Vec<u64>,
}

impl ColumnProfile {
    /// Summarises every column of `plane` into `buckets` row means.
    ///
    /// # Panics
    ///
    /// Panics if `buckets` is zero.
    #[must_use]
    pub fn new(plane: &LumaPlane, buckets: usize) -> Self {
        Self::new_in(plane, buckets, 0, plane.height())
    }

    pub(crate) fn new_in(plane: &LumaPlane, buckets: usize, first_row: u32, end_row: u32) -> Self {
        assert!(buckets > 0, "a column profile needs at least one bucket");
        assert!(
            first_row <= end_row && end_row <= plane.height(),
            "column-profile rows must be within the luma plane"
        );
        let selected_height = end_row - first_row;
        if selected_height == 0 {
            return Self {
                buckets: 1,
                width: plane.width(),
                data: vec![0; plane.width() as usize],
                sums: vec![0; plane.width() as usize],
            };
        }
        let buckets = buckets.min(selected_height as usize).max(1);
        let height = selected_height as usize;
        let width = plane.width() as usize;
        let mut sums = vec![0u64; width * buckets];
        let mut counts = vec![0u32; buckets];

        // Traverse the luma plane in its native row-major order. Walking one
        // column at a time turns a 5K frame into millions of cache misses.
        for selected_y in 0..height {
            let y = first_row as usize + selected_y;
            let bucket = (((selected_y + 1) * buckets - 1) / height).min(buckets - 1);
            counts[bucket] += 1;
            for (x, &value) in plane.row(y as u32).iter().enumerate() {
                sums[x * buckets + bucket] += u64::from(value);
            }
        }

        let mut data = vec![0u16; width * buckets];
        for x in 0..width {
            for bucket in 0..buckets {
                data[x * buckets + bucket] =
                    (sums[x * buckets + bucket] / u64::from(counts[bucket])) as u16;
            }
        }

        Self {
            buckets,
            width: plane.width(),
            data,
            sums,
        }
    }

    /// Number of row buckets per column.
    #[must_use]
    pub const fn buckets(&self) -> usize {
        self.buckets
    }

    /// Number of columns.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// One column's summary.
    ///
    /// # Panics
    ///
    /// Panics if `x` is outside the profile.
    #[must_use]
    pub fn column(&self, x: u32) -> &[u16] {
        let start = x as usize * self.buckets;
        &self.data[start..start + self.buckets]
    }

    /// Mean absolute difference between two columns, scaled to 0–255.
    #[must_use]
    pub fn column_distance(&self, x: u32, other: &Self, other_x: u32) -> u32 {
        let a = self.column(x);
        let b = other.column(other_x);
        let sum: u32 = a
            .iter()
            .zip(b)
            .map(|(&p, &q)| u32::from(p.abs_diff(q)))
            .sum();
        sum / a.len() as u32
    }

    pub(crate) fn column_pixel_sad_lower_bound(&self, x: u32, other: &Self, other_x: u32) -> u64 {
        let start = x as usize * self.buckets;
        let other_start = other_x as usize * other.buckets;
        self.sums[start..start + self.buckets]
            .iter()
            .zip(&other.sums[other_start..other_start + other.buckets])
            .map(|(&left, &right)| left.abs_diff(right))
            .sum()
    }
}

impl RowProfile {
    /// Summarises every row of `plane` into `buckets` column means.
    ///
    /// # Panics
    ///
    /// Panics if `buckets` is zero.
    #[must_use]
    pub fn new(plane: &LumaPlane, buckets: usize) -> Self {
        Self::new_in(plane, buckets, 0, plane.width())
    }

    pub(crate) fn new_in(
        plane: &LumaPlane,
        buckets: usize,
        first_column: u32,
        end_column: u32,
    ) -> Self {
        assert!(buckets > 0, "a row profile needs at least one bucket");
        assert!(
            first_column <= end_column && end_column <= plane.width(),
            "row-profile columns must be within the luma plane"
        );
        let selected_width = end_column - first_column;
        if selected_width == 0 {
            return Self {
                buckets: 1,
                height: plane.height(),
                data: vec![0; plane.height() as usize],
                sums: vec![0; plane.height() as usize],
            };
        }
        let buckets = buckets.min(selected_width as usize).max(1);
        let width = selected_width as usize;
        let mut data = vec![0u16; plane.height() as usize * buckets];
        let mut sums = vec![0u64; plane.height() as usize * buckets];

        for y in 0..plane.height() {
            let row = &plane.row(y)[first_column as usize..end_column as usize];
            let out = &mut data[y as usize * buckets..(y as usize + 1) * buckets];
            let row_sums = &mut sums[y as usize * buckets..(y as usize + 1) * buckets];
            for (b, slot) in out.iter_mut().enumerate() {
                // Integer bucket bounds, so a width that does not divide evenly
                // still partitions the row exactly once with no gaps.
                let start = b * width / buckets;
                let end = ((b + 1) * width / buckets).max(start + 1).min(width);
                let sum: u64 = row[start..end].iter().map(|&v| u64::from(v)).sum();
                row_sums[b] = sum;
                *slot = (sum / (end - start) as u64) as u16;
            }
        }

        Self {
            buckets,
            height: plane.height(),
            data,
            sums,
        }
    }

    /// Number of column buckets per row.
    #[must_use]
    pub const fn buckets(&self) -> usize {
        self.buckets
    }

    /// Number of rows.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// One row's summary.
    #[must_use]
    pub fn row(&self, y: u32) -> &[u16] {
        let start = y as usize * self.buckets;
        &self.data[start..start + self.buckets]
    }

    /// Mean absolute difference between two rows, scaled to 0–255.
    #[must_use]
    pub fn row_distance(&self, y: u32, other: &Self, other_y: u32) -> u32 {
        let a = self.row(y);
        let b = other.row(other_y);
        let sum: u32 = a
            .iter()
            .zip(b)
            .map(|(&p, &q)| u32::from(p.abs_diff(q)))
            .sum();
        sum / a.len() as u32
    }

    pub(crate) fn row_pixel_sad_lower_bound(&self, y: u32, other: &Self, other_y: u32) -> u64 {
        let start = y as usize * self.buckets;
        let other_start = other_y as usize * other.buckets;
        self.sums[start..start + self.buckets]
            .iter()
            .zip(&other.sums[other_start..other_start + other.buckets])
            .map(|(&left, &right)| left.abs_diff(right))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{ColorSpace, PhysicalSize, ScaleFactor};

    fn frame(
        format: PixelFormat,
        width: u32,
        height: u32,
        pixels: &[[u8; 4]],
        pad: usize,
    ) -> Frame {
        let stride = width as usize * 4 + pad;
        let mut data = vec![0xAA; stride * height as usize];
        for (i, px) in pixels.iter().enumerate() {
            let (x, y) = (i % width as usize, i / width as usize);
            data[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(px);
        }
        Frame {
            data,
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            stride,
            format,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::IDENTITY,
        }
    }

    #[test]
    fn channel_order_does_not_change_the_luma() {
        let rgba = frame(
            PixelFormat::Rgba8,
            2,
            1,
            &[[10, 20, 30, 255], [200, 100, 50, 255]],
            0,
        );
        let bgra = frame(
            PixelFormat::Bgra8,
            2,
            1,
            &[[30, 20, 10, 255], [50, 100, 200, 255]],
            0,
        );

        let a = LumaPlane::from_frame(&rgba).expect("rgba");
        let b = LumaPlane::from_frame(&bgra).expect("bgra");
        assert_eq!(
            a, b,
            "the same colours in two byte orders are the same grey"
        );
    }

    #[test]
    fn stride_padding_is_skipped_rather_than_read_as_pixels() {
        // The padding bytes are 0xAA. Reading them would show up immediately as
        // a bright column that is not in the image — the classic skew bug.
        let padded = frame(
            PixelFormat::Rgba8,
            2,
            2,
            &[[0; 4], [0; 4], [0; 4], [0; 4]],
            16,
        );
        let plane = LumaPlane::from_frame(&padded).expect("padded");
        assert_eq!(plane.data, vec![0, 0, 0, 0]);
    }

    #[test]
    fn premultiplied_colour_is_recovered_before_measuring_brightness() {
        // Mid-grey at 50% alpha, stored premultiplied, is byte value 64.
        let straight = frame(PixelFormat::Rgba8, 1, 1, &[[128, 128, 128, 255]], 0);
        let premul = frame(
            PixelFormat::RgbaPremultiplied8,
            1,
            1,
            &[[64, 64, 64, 128]],
            0,
        );

        let a = LumaPlane::from_frame(&straight).expect("straight");
        let b = LumaPlane::from_frame(&premul).expect("premultiplied");
        // Within the rounding of an integer divide, these are the same colour.
        assert!(a.row(0)[0].abs_diff(b.row(0)[0]) <= 1, "{a:?} vs {b:?}");
    }

    #[test]
    fn a_transparent_pixel_reads_the_same_in_both_frames() {
        let px = frame(PixelFormat::RgbaPremultiplied8, 1, 1, &[[0, 0, 0, 0]], 0);
        assert_eq!(LumaPlane::from_frame(&px).expect("clear").row(0)[0], 0);
    }

    #[test]
    fn a_short_buffer_is_refused_rather_than_indexed_past() {
        let mut bad = frame(PixelFormat::Rgba8, 4, 4, &[], 0);
        bad.data.truncate(10);
        let err = LumaPlane::from_frame(&bad).expect_err("a short buffer must not stitch");
        assert!(err.to_string().contains("too short"), "{err}");
    }

    #[test]
    fn an_empty_frame_is_refused() {
        let empty = frame(PixelFormat::Rgba8, 0, 0, &[], 0);
        assert!(LumaPlane::from_frame(&empty).is_err());
    }

    #[test]
    fn row_buckets_partition_the_row_exactly_once() {
        // 10 pixels into 4 buckets: no pixel counted twice, none dropped.
        let plane = LumaPlane::from_raw(10, 1, (0..10u8).map(|v| v * 10).collect());
        let profile = RowProfile::new(&plane, 4);
        assert_eq!(profile.buckets(), 4);
        // Buckets are [0,2) [2,5) [5,7) [7,10).
        assert_eq!(profile.row(0), [5, 30, 55, 80]);
    }

    #[test]
    fn asking_for_more_buckets_than_pixels_yields_one_per_pixel() {
        let plane = LumaPlane::from_raw(3, 1, vec![0, 128, 255]);
        let profile = RowProfile::new(&plane, 64);
        assert_eq!(profile.buckets(), 3);
        assert_eq!(profile.row(0), [0, 128, 255]);
    }

    #[test]
    fn identical_rows_are_zero_distance_apart() {
        let plane = LumaPlane::from_raw(4, 2, vec![1, 2, 3, 4, 1, 2, 3, 4]);
        let profile = RowProfile::new(&plane, 4);
        assert_eq!(profile.row_distance(0, &profile, 1), 0);
    }

    #[test]
    fn column_buckets_partition_the_column_exactly_once() {
        let plane = LumaPlane::from_raw(
            2,
            5,
            vec![
                0, 5, //
                20, 5, //
                40, 5, //
                60, 5, //
                80, 5,
            ],
        );
        let profile = ColumnProfile::new(&plane, 2);
        assert_eq!(profile.buckets(), 2);
        assert_eq!(profile.width(), 2);
        assert_eq!(profile.column(0), [10, 60]);
        assert_eq!(profile.column_distance(1, &profile, 1), 0);
    }

    #[test]
    fn profiles_tolerate_an_empty_perpendicular_axis() {
        let no_columns = LumaPlane::from_raw(0, 3, Vec::new());
        let no_rows = LumaPlane::from_raw(3, 0, Vec::new());

        assert_eq!(RowProfile::new(&no_columns, 4).row(1), [0]);
        assert_eq!(ColumnProfile::new(&no_rows, 4).column(1), [0]);
    }
}
