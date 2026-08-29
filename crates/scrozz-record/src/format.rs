//! Captured-pixel validation, scaling and NV12 conversion.

use scrozz_core::{Error, Result};

use crate::config::Dimensions;

/// Packed source pixel layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackedPixelFormat {
    /// Blue, green, red, alpha.
    Bgra,
    /// Blue, green, red, ignored.
    Bgrx,
    /// Red, green, blue, alpha.
    Rgba,
    /// Red, green, blue, ignored.
    Rgbx,
}

/// One owned packed capture frame.
#[derive(Debug, Clone)]
pub struct PackedFrame {
    /// Frame width.
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// Bytes between adjacent rows.
    pub stride: usize,
    /// Packed pixel layout.
    pub format: PackedPixelFormat,
    /// Pixel storage, including any row padding.
    pub data: Vec<u8>,
}

impl PackedFrame {
    /// Checks dimensions, stride and storage length.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for malformed external frame data.
    pub fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::InvalidRequest(
                "captured frame dimensions must be non-zero".into(),
            ));
        }
        let row_bytes = usize::try_from(self.width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or_else(|| Error::InvalidRequest("captured frame width overflows memory".into()))?;
        if self.stride < row_bytes {
            return Err(Error::InvalidRequest(format!(
                "captured frame stride {} is shorter than its {row_bytes}-byte row",
                self.stride
            )));
        }
        let required = self
            .stride
            .checked_mul(usize::try_from(self.height).unwrap_or(usize::MAX))
            .ok_or_else(|| Error::InvalidRequest("captured frame size overflows memory".into()))?;
        if self.data.len() < required {
            return Err(Error::InvalidRequest(format!(
                "captured frame has {} bytes but its dimensions require {required}",
                self.data.len()
            )));
        }
        Ok(())
    }

    fn rgb_at(&self, x: u32, y: u32) -> (u8, u8, u8) {
        let offset = y as usize * self.stride + x as usize * 4;
        let pixel = &self.data[offset..offset + 4];
        match self.format {
            PackedPixelFormat::Bgra | PackedPixelFormat::Bgrx => (pixel[2], pixel[1], pixel[0]),
            PackedPixelFormat::Rgba | PackedPixelFormat::Rgbx => (pixel[0], pixel[1], pixel[2]),
        }
    }
}

/// Encoder-ready NV12 frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nv12Frame {
    /// Frame width.
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// Full-resolution luma plane.
    pub y: Vec<u8>,
    /// Interleaved, half-resolution chroma plane.
    pub uv: Vec<u8>,
}

/// A packed frame positioned in a compositor-wide coordinate space.
#[derive(Debug, Clone, Copy)]
pub struct PlacedFrame<'a> {
    /// Global x coordinate.
    pub x: i32,
    /// Global y coordinate.
    pub y: i32,
    /// Captured pixels.
    pub frame: &'a PackedFrame,
}

/// Composites positioned portal streams into one RGBA desktop frame.
///
/// # Errors
///
/// Returns an error for no streams, malformed source frames, coordinate
/// overflow, or a composite larger than the encoder limit.
pub fn compose_placed_frames(frames: &[PlacedFrame<'_>]) -> Result<(PackedFrame, (i32, i32))> {
    if frames.is_empty() {
        return Err(Error::InvalidRequest(
            "at least one portal stream is required".into(),
        ));
    }
    for placed in frames {
        placed.frame.validate()?;
    }
    let left = frames.iter().map(|frame| i64::from(frame.x)).min().unwrap();
    let top = frames.iter().map(|frame| i64::from(frame.y)).min().unwrap();
    let right = frames
        .iter()
        .map(|placed| i64::from(placed.x) + i64::from(placed.frame.width))
        .max()
        .unwrap();
    let bottom = frames
        .iter()
        .map(|placed| i64::from(placed.y) + i64::from(placed.frame.height))
        .max()
        .unwrap();
    let width = u32::try_from(right - left)
        .map_err(|_| Error::InvalidRequest("portal stream width overflows coordinates".into()))?;
    let height = u32::try_from(bottom - top)
        .map_err(|_| Error::InvalidRequest("portal stream height overflows coordinates".into()))?;
    if width == 0 || height == 0 || width > Dimensions::MAX || height > Dimensions::MAX {
        return Err(Error::InvalidRequest(format!(
            "portal stream composite {width}x{height} is outside the encoder limits"
        )));
    }

    let stride = width as usize * 4;
    let mut output = PackedFrame {
        width,
        height,
        stride,
        format: PackedPixelFormat::Rgba,
        data: vec![0; stride * height as usize],
    };
    for placed in frames {
        let destination_x = usize::try_from(i64::from(placed.x) - left).unwrap();
        let destination_y = usize::try_from(i64::from(placed.y) - top).unwrap();
        for y in 0..placed.frame.height {
            for x in 0..placed.frame.width {
                let (red, green, blue) = placed.frame.rgb_at(x, y);
                let offset =
                    (destination_y + y as usize) * output.stride + (destination_x + x as usize) * 4;
                output.data[offset..offset + 4].copy_from_slice(&[red, green, blue, 255]);
            }
        }
    }
    Ok((
        output,
        (
            i32::try_from(left)
                .map_err(|_| Error::InvalidRequest("portal x coordinate exceeds i32".into()))?,
            i32::try_from(top)
                .map_err(|_| Error::InvalidRequest("portal y coordinate exceeds i32".into()))?,
        ),
    ))
}

/// Crops a packed frame without changing its channel layout.
///
/// # Errors
///
/// Returns an error when the crop has no area or extends beyond the source.
pub fn crop(source: &PackedFrame, x: u32, y: u32, width: u32, height: u32) -> Result<PackedFrame> {
    source.validate()?;
    if width == 0
        || height == 0
        || x.saturating_add(width) > source.width
        || y.saturating_add(height) > source.height
    {
        return Err(Error::InvalidRequest(format!(
            "crop {x},{y} {width}x{height} lies outside {}x{}",
            source.width, source.height
        )));
    }
    let row_bytes = width as usize * 4;
    let mut data = Vec::with_capacity(row_bytes * height as usize);
    for row in y..y + height {
        let start = row as usize * source.stride + x as usize * 4;
        data.extend_from_slice(&source.data[start..start + row_bytes]);
    }
    Ok(PackedFrame {
        width,
        height,
        stride: row_bytes,
        format: source.format,
        data,
    })
}

/// Scales a packed frame and converts it to limited-range BT.709 NV12.
///
/// Scaling deliberately uses nearest-neighbour sampling: capture is text-heavy,
/// the conversion stays dependency-free and deterministic, and native hardware
/// scaling can replace this path without changing encoder contracts.
///
/// # Errors
///
/// Returns an error for malformed input or odd/zero output dimensions.
pub fn to_nv12(source: &PackedFrame, output: Dimensions) -> Result<Nv12Frame> {
    source.validate()?;
    if output.width == 0
        || output.height == 0
        || !output.width.is_multiple_of(2)
        || !output.height.is_multiple_of(2)
    {
        return Err(Error::InvalidRequest(
            "NV12 output dimensions must be positive and even".into(),
        ));
    }

    let pixel_count = usize::try_from(output.width)
        .ok()
        .and_then(|width| width.checked_mul(output.height as usize))
        .ok_or_else(|| Error::InvalidRequest("NV12 output size overflows memory".into()))?;
    let mut y_plane = vec![0_u8; pixel_count];
    let mut uv_plane = vec![0_u8; pixel_count / 2];

    let scaled_rgb = |x: u32, y: u32| {
        let source_x = u64::from(x) * u64::from(source.width) / u64::from(output.width);
        let source_y = u64::from(y) * u64::from(source.height) / u64::from(output.height);
        source.rgb_at(source_x as u32, source_y as u32)
    };

    for y in 0..output.height {
        for x in 0..output.width {
            let (red, green, blue) = scaled_rgb(x, y);
            y_plane[y as usize * output.width as usize + x as usize] = rgb_to_y(red, green, blue);
        }
    }

    for y in (0..output.height).step_by(2) {
        for x in (0..output.width).step_by(2) {
            let mut red = 0_u32;
            let mut green = 0_u32;
            let mut blue = 0_u32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let pixel = scaled_rgb(x + dx, y + dy);
                    red += u32::from(pixel.0);
                    green += u32::from(pixel.1);
                    blue += u32::from(pixel.2);
                }
            }
            let red = (red / 4) as u8;
            let green = (green / 4) as u8;
            let blue = (blue / 4) as u8;
            let chroma_offset = (y as usize / 2) * output.width as usize + x as usize;
            uv_plane[chroma_offset] = rgb_to_u(red, green, blue);
            uv_plane[chroma_offset + 1] = rgb_to_v(red, green, blue);
        }
    }

    Ok(Nv12Frame {
        width: output.width,
        height: output.height,
        y: y_plane,
        uv: uv_plane,
    })
}

fn clamp_byte(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

fn rgb_to_y(red: u8, green: u8, blue: u8) -> u8 {
    // BT.709 limited-range coefficients, represented as 8-bit fixed point.
    clamp_byte(
        16 + (47 * i32::from(red) + 157 * i32::from(green) + 16 * i32::from(blue) + 128) / 256,
    )
}

fn rgb_to_u(red: u8, green: u8, blue: u8) -> u8 {
    clamp_byte(
        128 + (-26 * i32::from(red) - 87 * i32::from(green) + 112 * i32::from(blue) + 128) / 256,
    )
}

fn rgb_to_v(red: u8, green: u8, blue: u8) -> u8 {
    clamp_byte(
        128 + (112 * i32::from(red) - 102 * i32::from(green) - 10 * i32::from(blue) + 128) / 256,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        PackedFrame, PackedPixelFormat, PlacedFrame, compose_placed_frames, crop, to_nv12,
    };
    use crate::config::Dimensions;

    #[test]
    fn validates_padded_stride_and_rejects_short_storage() {
        let mut frame = PackedFrame {
            width: 2,
            height: 2,
            stride: 12,
            format: PackedPixelFormat::Rgba,
            data: vec![0; 24],
        };
        assert!(frame.validate().is_ok());
        frame.data.pop();
        assert!(frame.validate().is_err());
    }

    #[test]
    fn conversion_respects_channel_order_and_nv12_shape() {
        let frame = PackedFrame {
            width: 2,
            height: 2,
            stride: 8,
            format: PackedPixelFormat::Bgra,
            data: vec![
                0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255,
            ],
        };
        let nv12 = to_nv12(
            &frame,
            Dimensions {
                width: 2,
                height: 2,
            },
        )
        .unwrap();
        assert_eq!(nv12.y, vec![63; 4]);
        assert_eq!(nv12.uv.len(), 2);
        assert!(nv12.uv[1] > 200);
    }

    #[test]
    fn conversion_scales_deterministically() {
        let frame = PackedFrame {
            width: 1,
            height: 1,
            stride: 4,
            format: PackedPixelFormat::Rgba,
            data: vec![255, 255, 255, 255],
        };
        let nv12 = to_nv12(
            &frame,
            Dimensions {
                width: 4,
                height: 2,
            },
        )
        .unwrap();
        assert_eq!(nv12.y, vec![235; 8]);
        assert_eq!(nv12.uv, vec![128; 4]);
    }

    #[test]
    fn composes_negative_monitor_origins() {
        let red = PackedFrame {
            width: 2,
            height: 2,
            stride: 8,
            format: PackedPixelFormat::Rgba,
            data: [255, 0, 0, 255].repeat(4),
        };
        let blue = PackedFrame {
            width: 2,
            height: 2,
            stride: 8,
            format: PackedPixelFormat::Bgra,
            data: [255, 0, 0, 255].repeat(4),
        };
        let (composite, origin) = compose_placed_frames(&[
            PlacedFrame {
                x: -2,
                y: 0,
                frame: &red,
            },
            PlacedFrame {
                x: 0,
                y: 0,
                frame: &blue,
            },
        ])
        .unwrap();
        assert_eq!(origin, (-2, 0));
        assert_eq!((composite.width, composite.height), (4, 2));
        assert_eq!(&composite.data[..4], &[255, 0, 0, 255]);
        assert_eq!(&composite.data[8..12], &[0, 0, 255, 255]);
    }

    #[test]
    fn crops_with_padded_source_rows() {
        let source = PackedFrame {
            width: 2,
            height: 2,
            stride: 12,
            format: PackedPixelFormat::Rgba,
            data: vec![
                1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 9, 10, 11, 12, 13, 14, 15, 16, 0, 0, 0, 0,
            ],
        };
        let cropped = crop(&source, 1, 0, 1, 2).unwrap();
        assert_eq!(cropped.data, vec![5, 6, 7, 8, 13, 14, 15, 16]);
        assert_eq!(cropped.stride, 4);
    }
}
