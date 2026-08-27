//! Pure recording-plan arithmetic.

use crate::Quality;

/// The encoder-facing dimensions and rate selected for a capture source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderPlan {
    /// Fixed BGRA input width.
    pub source_width: u32,
    /// Fixed BGRA input height.
    pub source_height: u32,
    /// H.264 output width.
    pub output_width: u32,
    /// H.264 output height.
    pub output_height: u32,
    /// Frames per second.
    pub fps: u32,
    /// Target H.264 bitrate in bits per second.
    pub bitrate: u32,
    /// Key-frame interval in frames.
    pub gop: u32,
}

/// A recording plan cannot be represented by Media Foundation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The target has no pixels.
    EmptySource,
    /// The frame rate is outside the public contract.
    InvalidFrameRate(u32),
    /// A requested height cap is too small for H.264.
    InvalidHeightCap(u32),
}

/// Builds a deterministic encoder plan.
pub fn build(
    width: u32,
    height: u32,
    fps: u32,
    quality: Quality,
    max_height: Option<u32>,
) -> Result<EncoderPlan, PlanError> {
    if width == 0 || height == 0 {
        return Err(PlanError::EmptySource);
    }
    if !(1..=240).contains(&fps) {
        return Err(PlanError::InvalidFrameRate(fps));
    }
    if matches!(max_height, Some(0 | 1)) {
        return Err(PlanError::InvalidHeightCap(max_height.unwrap_or_default()));
    }

    let (output_width, output_height) = output_dimensions(width, height, max_height);
    let bits_per_pixel = match quality {
        Quality::Low => 0.04,
        Quality::Balanced => 0.09,
        Quality::High => 0.18,
    };
    let raw_bitrate =
        f64::from(output_width) * f64::from(output_height) * f64::from(fps) * bits_per_pixel;
    let bitrate = raw_bitrate.clamp(128_000.0, 80_000_000.0).round() as u32;

    Ok(EncoderPlan {
        source_width: width,
        source_height: height,
        output_width,
        output_height,
        fps,
        bitrate,
        gop: fps.saturating_mul(2),
    })
}

/// Applies a height ceiling without upscaling and rounds for 4:2:0 H.264.
#[must_use]
pub fn output_dimensions(width: u32, height: u32, max_height: Option<u32>) -> (u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    let (scaled_width, scaled_height) = match max_height {
        Some(cap) if cap >= 2 && height > cap => {
            let numerator = u64::from(width) * u64::from(cap);
            let scaled_width = ((numerator + u64::from(height) / 2) / u64::from(height)) as u32;
            (scaled_width.max(1), cap)
        }
        _ => (width, height),
    };
    (even(scaled_width), even(scaled_height))
}

const fn even(value: u32) -> u32 {
    let value = value & !1;
    if value < 2 { 2 } else { value }
}
