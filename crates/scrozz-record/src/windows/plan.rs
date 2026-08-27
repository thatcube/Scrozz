//! Pure recording-plan arithmetic.

use crate::{Quality, RecordingResolution};

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
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlanError {
    /// The target has no pixels.
    EmptySource,
    /// The frame rate is outside the public contract.
    InvalidFrameRate(u32),
    /// The source scale is not usable for logical-point sizing.
    InvalidScale(f64),
}

/// Builds a deterministic encoder plan.
pub fn build(
    width: u32,
    height: u32,
    backing_scale: f64,
    fps: u32,
    quality: Quality,
    resolution: RecordingResolution,
) -> Result<EncoderPlan, PlanError> {
    if width == 0 || height == 0 {
        return Err(PlanError::EmptySource);
    }
    if !(1..=240).contains(&fps) {
        return Err(PlanError::InvalidFrameRate(fps));
    }
    if !backing_scale.is_finite() || backing_scale <= 0.0 {
        return Err(PlanError::InvalidScale(backing_scale));
    }

    let (output_width, output_height) = output_dimensions(width, height, backing_scale, resolution);
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

/// Applies the shared resolution policy and guarantees GPU-safe nonzero even dimensions.
#[must_use]
pub fn output_dimensions(
    width: u32,
    height: u32,
    backing_scale: f64,
    resolution: RecordingResolution,
) -> (u32, u32) {
    let (scaled_width, scaled_height) = resolution.apply(width, height, backing_scale);
    (nonzero_even(scaled_width), nonzero_even(scaled_height))
}

const fn nonzero_even(value: u32) -> u32 {
    let even = value & !1;
    if even < 2 { 2 } else { even }
}
