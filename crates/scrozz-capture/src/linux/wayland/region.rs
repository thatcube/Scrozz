//! Placing a user-drawn region inside a portal-supplied monitor stream.
//!
//! The ScreenCast portal has no concept of a sub-rectangle. It hands back whole
//! monitors, so a region capture is a monitor capture followed by a crop — and
//! the crop is where the arithmetic can quietly go wrong, which is why it lives
//! here as plain values and is tested on every platform.
//!
//! # Three coordinate spaces, and the one thing that reconciles them
//!
//! - Scrozz asks for a rectangle in the **global logical desktop**, because
//!   that is what the selection overlay draws in.
//! - The portal reports each stream's `position` and `size` in the
//!   **compositor's logical space** — the same space, offset to the monitor.
//! - PipeWire delivers **physical pixels**, and under scaling there are more of
//!   them than the logical size suggests.
//!
//! The tempting shortcut is to use the display's scale factor for the last
//! conversion. It is wrong often enough to matter: under fractional scaling the
//! compositor rounds the pixel size of an output, so a 1.25× scaled 1920-logical
//! monitor is not reliably 2400 pixels wide, and XWayland — the only place this
//! backend can read a scale from — reports a rounded integer scale anyway.
//!
//! What is always true is that the frame and the portal's `size` describe the
//! *same* monitor. Dividing one by the other recovers the real ratio the
//! compositor used, whatever it was. That is the number this module uses.
//!
//! # When it cannot be done
//!
//! `position` and `size` are optional in the portal specification and genuinely
//! absent on some backends. Without them a region cannot be located, and the
//! honest response is to say so: returning the whole monitor instead would hand
//! back an image the user did not ask for and had no way to notice was wrong.

use scrozz_core::{Frame, LogicalRect, PhysicalSize, ScaleFactor};

use super::portal::StreamInfo;

/// A crop rectangle in frame-local pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CropRect {
    /// Left edge, in pixels from the frame's left.
    pub x: u32,
    /// Top edge, in pixels from the frame's top.
    pub y: u32,
    /// Width in pixels. Always non-zero.
    pub width: u32,
    /// Height in pixels. Always non-zero.
    pub height: u32,
}

/// Why a region could not be placed inside a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CropError {
    /// The portal reported no position or no size for the stream.
    NoGeometry,
    /// The portal reported a zero or negative size.
    DegenerateStream,
    /// The region does not overlap the captured monitor at all.
    Outside,
}

impl CropError {
    /// Turns the failure into the error a caller should see.
    #[must_use]
    pub fn into_error(self, compositor: &str) -> scrozz_core::Error {
        use scrozz_core::Error;

        match self {
            Self::NoGeometry => Error::Unsupported {
                what: "capturing a region on Wayland".into(),
                why: format!(
                    "the portal on {compositor} granted a screen-cast stream but reported neither \
                     its position nor its size, so there is no way to tell which part of the \
                     image the selected region corresponds to. Capture the whole display instead, \
                     or crop afterwards in the editor"
                ),
            },
            Self::DegenerateStream => Error::Platform(format!(
                "the portal on {compositor} reported a stream with no area, which leaves nothing \
                 to crop from"
            )),
            Self::Outside => scrozz_core::Error::InvalidRequest(
                "the selected region lies entirely outside the display that was captured".into(),
            ),
        }
    }
}

/// Works out which pixels of `frame_size` correspond to `region`.
///
/// `frame_size` is the negotiated pixel size of the stream, not the portal's
/// logical size; the ratio between the two is the scale actually in force.
///
/// # Errors
///
/// [`CropError::NoGeometry`] when the portal supplied no placement,
/// [`CropError::DegenerateStream`] when it supplied an empty one, and
/// [`CropError::Outside`] when the region misses the monitor entirely.
pub fn plan_crop(
    region: LogicalRect,
    stream: &StreamInfo,
    frame_size: (u32, u32),
) -> Result<CropRect, CropError> {
    let (Some((origin_x, origin_y)), Some((logical_w, logical_h))) = (stream.position, stream.size)
    else {
        return Err(CropError::NoGeometry);
    };

    if logical_w <= 0 || logical_h <= 0 || frame_size.0 == 0 || frame_size.1 == 0 {
        return Err(CropError::DegenerateStream);
    }

    let scale_x = f64::from(frame_size.0) / f64::from(logical_w);
    let scale_y = f64::from(frame_size.1) / f64::from(logical_h);

    // Local logical coordinates, then pixels. Outward rounding: a region is a
    // selection, and losing a row of the thing the user framed is more annoying
    // than gaining one of the background.
    let left = ((region.origin.x - f64::from(origin_x)) * scale_x).floor();
    let top = ((region.origin.y - f64::from(origin_y)) * scale_y).floor();
    let right = ((region.origin.x + region.size.width - f64::from(origin_x)) * scale_x).ceil();
    let bottom = ((region.origin.y + region.size.height - f64::from(origin_y)) * scale_y).ceil();

    if !(left.is_finite() && top.is_finite() && right.is_finite() && bottom.is_finite()) {
        return Err(CropError::Outside);
    }

    // Clamp into the frame before converting, so a region hanging off the left
    // edge becomes a smaller rectangle rather than a negative origin.
    let clamped_left = left.max(0.0).min(f64::from(frame_size.0));
    let clamped_top = top.max(0.0).min(f64::from(frame_size.1));
    let clamped_right = right.max(0.0).min(f64::from(frame_size.0));
    let clamped_bottom = bottom.max(0.0).min(f64::from(frame_size.1));

    if clamped_right <= clamped_left || clamped_bottom <= clamped_top {
        return Err(CropError::Outside);
    }

    Ok(CropRect {
        x: clamped_left as u32,
        y: clamped_top as u32,
        width: (clamped_right - clamped_left) as u32,
        height: (clamped_bottom - clamped_top) as u32,
    })
}

/// Copies `rect` out of `frame` into a new tightly-packed frame.
///
/// Returns [`CropError::Outside`] if the rectangle does not lie wholly within
/// the frame — which [`plan_crop`] guarantees, but this is the last place a
/// mistake could become an out-of-bounds read, so it is checked rather than
/// assumed.
///
/// # Errors
///
/// [`CropError::Outside`] when `rect` escapes the frame or the frame's buffer is
/// shorter than its declared geometry.
pub fn crop(frame: &Frame, rect: CropRect) -> Result<Frame, CropError> {
    let bpp = frame.format.bytes_per_pixel();
    let width = frame.width();
    let height = frame.height();

    if rect.width == 0
        || rect.height == 0
        || rect.x.saturating_add(rect.width) > width
        || rect.y.saturating_add(rect.height) > height
    {
        return Err(CropError::Outside);
    }

    let out_stride = rect.width as usize * bpp;
    let mut data = Vec::with_capacity(out_stride * rect.height as usize);

    for row in 0..rect.height as usize {
        let start = (rect.y as usize + row) * frame.stride + rect.x as usize * bpp;
        let end = start + out_stride;
        let Some(slice) = frame.data.get(start..end) else {
            return Err(CropError::Outside);
        };
        data.extend_from_slice(slice);
    }

    Ok(Frame {
        data,
        size: PhysicalSize::new(f64::from(rect.width), f64::from(rect.height)),
        stride: out_stride,
        format: frame.format,
        color_space: frame.color_space,
        scale: frame.scale,
    })
}

/// Recovers the scale factor the compositor actually applied to a stream.
///
/// PipeWire reports pixels and the portal reports logical units, so their ratio
/// *is* the scale — no guessing, no rounding assumptions, and correct under
/// fractional scaling where the display's advertised scale is not.
///
/// `fallback` is used when the portal supplied no size, which is the only case
/// where there is nothing to divide.
#[must_use]
pub fn resolve_scale(
    stream: &StreamInfo,
    frame_size: (u32, u32),
    fallback: ScaleFactor,
) -> ScaleFactor {
    let Some((logical_w, logical_h)) = stream.size else {
        return fallback;
    };
    if logical_w <= 0 || logical_h <= 0 || frame_size.0 == 0 || frame_size.1 == 0 {
        return fallback;
    }

    // Horizontal and vertical scales are equal on every compositor in practice;
    // averaging rather than picking one keeps a rounded-by-one output from
    // biasing the result in a single axis.
    let ratio = (f64::from(frame_size.0) / f64::from(logical_w)
        + f64::from(frame_size.1) / f64::from(logical_h))
        / 2.0;

    if ratio.is_finite() && ratio > 0.0 {
        ScaleFactor::new(ratio)
    } else {
        fallback
    }
}
