//! Bounded live previews, sampled without duplicating the full capture.

use scrozz_core::{Error, Frame, PixelFormat, Result};

/// Maximum edge of a live preview, independent of the full capture's size.
pub const PREVIEW_MAX_EDGE: u32 = 768;

/// Last matched viewport, measured in full stitched-image pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewViewport {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width of captured content, excluding removed fixed chrome.
    pub width: u32,
    /// Height of captured content, excluding removed fixed chrome.
    pub height: u32,
}

impl PreviewViewport {
    /// The complete initial frame before any content is appended.
    #[must_use]
    pub const fn full(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }
}

/// A small, straight-alpha RGBA view of the currently accepted capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollPreview {
    /// Preview width in pixels.
    pub width: u32,
    /// Preview height in pixels.
    pub height: u32,
    /// Original width, used to preserve the exact aspect ratio while fitting.
    pub source_width: u32,
    /// Original height.
    pub source_height: u32,
    /// The section matching the most recently accepted viewport.
    pub viewport: PreviewViewport,
    /// Row-major straight-alpha RGBA pixels.
    pub rgba: Vec<u8>,
}

impl ScrollPreview {
    /// Samples the initial viewport before a seam has been accepted.
    pub fn from_frame(frame: &Frame) -> Result<Self> {
        if !frame.is_well_formed() || frame.width() == 0 || frame.height() == 0 {
            return Err(Error::InvalidRequest(
                "cannot preview a malformed scrolling frame".into(),
            ));
        }
        Ok(sample_preview(
            frame.width(),
            frame.height(),
            frame.format,
            |x, y| {
                let offset = y as usize * frame.stride + x as usize * 4;
                frame.data[offset..offset + 4]
                    .try_into()
                    .expect("validated RGBA pixel")
            },
        ))
    }
}

pub(crate) fn sample_preview(
    source_width: u32,
    source_height: u32,
    format: PixelFormat,
    mut pixel: impl FnMut(u32, u32) -> [u8; 4],
) -> ScrollPreview {
    let longest = source_width.max(source_height).max(1);
    let edge = longest.min(PREVIEW_MAX_EDGE);
    let width = ((u64::from(source_width) * u64::from(edge)) / u64::from(longest)).max(1) as u32;
    let height = ((u64::from(source_height) * u64::from(edge)) / u64::from(longest)).max(1) as u32;
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        let source_y = (u64::from(y) * u64::from(source_height) / u64::from(height)) as u32;
        for x in 0..width {
            let source_x = (u64::from(x) * u64::from(source_width) / u64::from(width)) as u32;
            let mut px = pixel(source_x, source_y);
            if matches!(format, PixelFormat::Bgra8 | PixelFormat::BgraPremultiplied8) {
                px.swap(0, 2);
            }
            if format.is_premultiplied() {
                let alpha = u32::from(px[3]);
                for channel in &mut px[..3] {
                    *channel = (u32::from(*channel) * 255 + alpha / 2)
                        .checked_div(alpha)
                        .unwrap_or(0)
                        .min(255) as u8;
                }
            }
            rgba.extend_from_slice(&px);
        }
    }
    ScrollPreview {
        width,
        height,
        source_width,
        source_height,
        viewport: PreviewViewport::full(source_width, source_height),
        rgba,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_storage_is_bounded_for_every_aspect_ratio() {
        for (width, height) in [(20, 90_000), (90_000, 20), (8_000, 8_000), (2, 2)] {
            let preview = sample_preview(width, height, PixelFormat::Rgba8, |_, _| [1, 2, 3, 255]);
            assert!(preview.width <= PREVIEW_MAX_EDGE && preview.height <= PREVIEW_MAX_EDGE);
            assert_eq!(
                preview.rgba.len(),
                preview.width as usize * preview.height as usize * 4
            );
            assert_eq!(
                (preview.source_width, preview.source_height),
                (width, height)
            );
        }
    }

    #[test]
    fn preview_converts_premultiplied_bgra_without_dark_or_swapped_edges() {
        let preview = sample_preview(1, 1, PixelFormat::BgraPremultiplied8, |_, _| {
            [16, 32, 64, 128]
        });
        assert_eq!(preview.rgba, vec![128, 64, 32, 128]);
    }
}
