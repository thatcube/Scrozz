//! Turning a captured [`Frame`] into something an OCR engine can actually read.
//!
//! # Why a whole module for "convert the pixels"
//!
//! Screenshots are not documents, and every assumption a document-OCR pipeline
//! makes is wrong here:
//!
//! - They are **small**. A dragged region is routinely 400×120. Document OCR
//!   engines assume 300 DPI scans; at 1× a screenshot is closer to 72 DPI.
//! - They are **strided**. Capture APIs pad rows to an alignment boundary, so
//!   `data` is almost never `width * 4` bytes per row.
//! - They arrive in **three different byte layouts**, one of which is
//!   premultiplied. Feeding premultiplied samples to an engine that expects
//!   straight alpha darkens every antialiased glyph edge, which is precisely the
//!   text the engine is least sure about.
//!
//! So this module normalises to one canonical form — tightly packed, straight
//! alpha, RGBA byte order — and then, crucially, **upscales small images before
//! recognition**. That last step is not a nicety. On a 1× display it is the
//! difference between usable results and a near-empty list, because the system
//! engines are tuned for text that is tens of pixels tall and UI text at 1× is
//! eleven.
//!
//! Everything here is platform-independent and therefore testable everywhere,
//! which matters: it is the half of OCR that CI on any runner can actually
//! verify.

use scrozz_core::{Error, Frame, PhysicalSize, PixelFormat, Result, ScaleFactor};

/// The effective device scale we try to present to the recognition engine.
///
/// Text on a 2× display is around 26 physical pixels tall, which every system
/// engine handles comfortably. At 1× the same text is 13 pixels and results fall
/// off a cliff, so a 1× frame is doubled to meet this target.
const TARGET_EFFECTIVE_SCALE: f64 = 2.0;

/// Below this short edge, in physical pixels, an image is upscaled regardless of
/// its display scale.
///
/// Engines express their minimum detectable text height as a *fraction of image
/// height*, so a small image penalises small text twice over. A 300-pixel-tall
/// crop is enlarged even when it came from a Retina display.
const MIN_SHORT_EDGE: f64 = 640.0;

/// The largest upscale ever applied.
///
/// Past roughly 4× there is no new information to recover — only interpolation
/// artefacts and quadratically more work.
const MAX_UPSCALE: u32 = 4;

/// Ceiling on the prepared image's pixel count.
///
/// Bounds worst-case memory: the resampler holds one intermediate buffer, so the
/// peak is about `8` bytes per output pixel.
const MAX_OUTPUT_PIXELS: u64 = 16_000_000;

/// How aggressively to enlarge an image before recognition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpscalePolicy {
    /// Choose a factor from the frame's size and display scale.
    #[default]
    Automatic,
    /// Never resample. Useful when a caller has already prepared the image.
    Off,
    /// Always use this factor, clamped to [`MAX_UPSCALE`] and the pixel budget.
    Fixed(u32),
}

/// A tightly packed, straight-alpha, RGBA byte-order image.
///
/// The one shape every backend converts *from*, so each backend's remaining work
/// is a single well-understood swizzle rather than a matrix of format cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba8Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `height * width * 4` bytes, no row padding.
    pub data: Vec<u8>,
}

/// Converts straight-alpha RGBA pixels to Rec.601 luma over opaque white.
pub(crate) fn rec601_luma_on_white(image: &Rgba8Image) -> Vec<u8> {
    image
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| {
            let red = u32::from(pixel[0]);
            let green = u32::from(pixel[1]);
            let blue = u32::from(pixel[2]);
            let alpha = u32::from(pixel[3]);
            let luma = (299 * red + 587 * green + 114 * blue + 500) / 1_000;
            ((luma * alpha + 255 * (255 - alpha) + 127) / 255) as u8
        })
        .collect()
}

impl Rgba8Image {
    /// Creates an image, checking that `data` matches `width * height * 4`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if either dimension is zero or the
    /// buffer length is wrong.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Result<Self> {
        // A zero-sized image is never something a caller wants: it silently
        // recognises nothing, which reads as "no text here" rather than as the
        // mistake it is.
        if width == 0 || height == 0 {
            return Err(Error::InvalidRequest(format!(
                "rgba8 image has a zero dimension: {width}x{height}"
            )));
        }
        let expected = width as usize * height as usize * 4;
        if data.len() != expected {
            return Err(Error::InvalidRequest(format!(
                "rgba8 buffer is {} bytes, expected {expected} for {width}x{height}",
                data.len()
            )));
        }
        Ok(Self {
            width,
            height,
            data,
        })
    }

    /// Normalises a captured frame: drops row padding, reorders channels and
    /// undoes premultiplication.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the frame is empty or its buffer is
    /// too short for its declared geometry.
    pub fn from_frame(frame: &Frame) -> Result<Self> {
        let (width, height) = (frame.width(), frame.height());
        if width == 0 || height == 0 {
            return Err(Error::InvalidRequest(format!(
                "cannot recognise text in a {width}x{height} frame"
            )));
        }
        if !frame.is_well_formed() {
            return Err(Error::InvalidRequest(format!(
                "frame buffer is {} bytes, too short for {width}x{height} at stride {}",
                frame.data.len(),
                frame.stride
            )));
        }

        let row_len = width as usize * 4;
        let mut data = vec![0u8; row_len * height as usize];
        // Which source byte holds red, and whether colour is scaled by alpha.
        // Resolved once rather than per pixel.
        let (r_at, b_at) = match frame.format {
            PixelFormat::Rgba8 | PixelFormat::RgbaPremultiplied8 => (0, 2),
            PixelFormat::Bgra8 | PixelFormat::BgraPremultiplied8 => (2, 0),
        };
        let premultiplied = frame.format.is_premultiplied();

        for y in 0..height as usize {
            let src = &frame.data[y * frame.stride..y * frame.stride + row_len];
            let dst = &mut data[y * row_len..(y + 1) * row_len];
            for (s, d) in src
                .as_chunks::<4>()
                .0
                .iter()
                .zip(dst.as_chunks_mut::<4>().0.iter_mut())
            {
                let a = s[3];
                let (r, g, b) = (s[r_at], s[1], s[b_at]);
                let (r, g, b) = if premultiplied {
                    (
                        unpremultiply(r, a),
                        unpremultiply(g, a),
                        unpremultiply(b, a),
                    )
                } else {
                    (r, g, b)
                };
                d[0] = r;
                d[1] = g;
                d[2] = b;
                d[3] = a;
            }
        }

        Ok(Self {
            width,
            height,
            data,
        })
    }

    /// Resamples to `width` × `height` with a Catmull-Rom kernel.
    ///
    /// Catmull-Rom rather than bilinear because it keeps the sharp light/dark
    /// transition at a glyph edge, which is the only signal an OCR engine has.
    /// Bilinear turns a two-pixel stroke into a grey smear. When downscaling the
    /// kernel widens to cover the whole source footprint, so shrinking averages
    /// rather than point-samples and thin strokes do not simply vanish.
    ///
    /// Returns a clone when the size is unchanged.
    #[must_use]
    pub fn resample(&self, width: u32, height: u32) -> Self {
        if width == self.width && height == self.height {
            return self.clone();
        }
        if width == 0 || height == 0 || self.width == 0 || self.height == 0 {
            return Self {
                width: 0,
                height: 0,
                data: Vec::new(),
            };
        }

        // Horizontal first, then vertical, over an 8-bit intermediate. Two
        // separable passes are O(n·r) instead of O(n·r²), and 8 bits of
        // intermediate precision is far below the noise floor of anything OCR
        // decides.
        let horizontal = resample_axis(
            &self.data,
            self.width as usize,
            self.height as usize,
            width as usize,
            Axis::Horizontal,
        );
        let vertical = resample_axis(
            &horizontal,
            width as usize,
            self.height as usize,
            height as usize,
            Axis::Vertical,
        );

        Self {
            width,
            height,
            data: vertical,
        }
    }
}

/// An image ready for an engine, plus the factor needed to map results back.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The normalised, possibly resampled image.
    pub image: Rgba8Image,
    /// Prepared pixels per original physical pixel.
    ///
    /// Every coordinate an engine reports is in the prepared image's space.
    /// Dividing by this is what puts a highlight box over the right pixels
    /// instead of somewhere nearby.
    pub upscale: f64,
    /// The original frame's size, so results can be mapped back without also
    /// carrying the frame around.
    pub source_size: PhysicalSize,
}

/// Normalises and, if worthwhile, enlarges a frame for recognition.
///
/// `max_dimension` is an engine's hard limit on either side, if it has one;
/// exceeding it is not an error, the image is shrunk to fit and coordinates are
/// mapped back through [`Prepared::upscale`] exactly as for an enlargement.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] if the frame is empty or malformed.
pub fn prepare(
    frame: &Frame,
    policy: UpscalePolicy,
    max_dimension: Option<u32>,
) -> Result<Prepared> {
    let image = Rgba8Image::from_frame(frame)?;
    let factor = upscale_factor(
        image.width,
        image.height,
        frame.scale,
        policy,
        max_dimension,
    );

    let image = if (factor - 1.0).abs() < f64::EPSILON {
        image
    } else {
        let width = ((f64::from(image.width) * factor).round() as u32).max(1);
        let height = ((f64::from(image.height) * factor).round() as u32).max(1);
        image.resample(width, height)
    };

    Ok(Prepared {
        image,
        upscale: factor,
        source_size: frame.size,
    })
}

/// Decides how much to scale an image before recognition.
///
/// Two independent reasons to enlarge, and the stronger wins:
///
/// 1. **Low display scale.** Glyph height in pixels is roughly point size times
///    scale, so a 1× frame needs doubling to look like the 2× frames these
///    engines were tuned on.
/// 2. **Small absolute size.** Engines gate on text height as a fraction of
///    image height, which punishes small crops independently of their scale.
///
/// The result is then clamped by [`MAX_UPSCALE`], the pixel budget, and any
/// engine size limit — and that last clamp can legitimately drive it below 1.0,
/// shrinking an oversized capture rather than refusing it.
#[must_use]
pub fn upscale_factor(
    width: u32,
    height: u32,
    scale: ScaleFactor,
    policy: UpscalePolicy,
    max_dimension: Option<u32>,
) -> f64 {
    if width == 0 || height == 0 {
        return 1.0;
    }
    let (w, h) = (f64::from(width), f64::from(height));

    let wanted = match policy {
        UpscalePolicy::Off => 1.0,
        UpscalePolicy::Fixed(n) => f64::from(n.max(1)),
        UpscalePolicy::Automatic => {
            let for_density = TARGET_EFFECTIVE_SCALE / scale.get();
            let for_size = MIN_SHORT_EDGE / w.min(h);
            // Integer factors keep the resampler on exact pixel centres, which
            // is measurably kinder to hairline strokes than 1.7×.
            for_density.max(for_size).max(1.0).ceil()
        }
    };
    let mut factor = wanted.clamp(1.0, f64::from(MAX_UPSCALE));

    // Budget first: an enlargement we cannot afford is worse than none. This
    // only ever holds an *upscale* back — a capture that is already larger than
    // the budget is passed through untouched, because throwing away pixels the
    // user already has costs accuracy and buys nothing. Only a hard engine
    // ceiling below is allowed to shrink.
    let budget = (MAX_OUTPUT_PIXELS as f64 / (w * h)).sqrt().max(1.0);
    if factor > budget {
        factor = budget;
    }

    // Then the engine's own ceiling, which may shrink us below 1.0.
    if let Some(max) = max_dimension
        && max > 0
    {
        let limit = f64::from(max) / w.max(h);
        if factor > limit {
            factor = limit;
        }
    }

    // Snap near-identity back to exactly 1.0 so callers can skip resampling.
    if (factor - 1.0).abs() < 1e-9 {
        1.0
    } else {
        factor
    }
}

/// Recovers a straight-alpha channel from a premultiplied one.
fn unpremultiply(channel: u8, alpha: u8) -> u8 {
    match alpha {
        0 => 0,
        255 => channel,
        a => {
            let value = (u32::from(channel) * 255 + u32::from(a) / 2) / u32::from(a);
            value.min(255) as u8
        }
    }
}

/// Which axis a resampling pass operates on.
#[derive(Clone, Copy)]
enum Axis {
    Horizontal,
    Vertical,
}

/// The Catmull-Rom kernel, support radius 2.
fn catmull_rom(x: f64) -> f64 {
    let x = x.abs();
    if x < 1.0 {
        1.5 * x * x * x - 2.5 * x * x + 1.0
    } else if x < 2.0 {
        -0.5 * x * x * x + 2.5 * x * x - 4.0 * x + 2.0
    } else {
        0.0
    }
}

/// Per-output-sample source range and weights.
struct Weights {
    first: usize,
    values: Vec<f32>,
}

/// Precomputes filter taps mapping `src_len` samples to `dst_len`.
fn weights_for(src_len: usize, dst_len: usize) -> Vec<Weights> {
    let ratio = dst_len as f64 / src_len as f64;
    // Downscaling widens the kernel so every source pixel contributes; without
    // this, shrinking point-samples and a one-pixel stroke can land between taps.
    let filter_scale = if ratio < 1.0 { 1.0 / ratio } else { 1.0 };
    let radius = 2.0 * filter_scale;

    (0..dst_len)
        .map(|i| {
            let center = (i as f64 + 0.5) / ratio - 0.5;
            let first = (center - radius).ceil().max(0.0) as usize;
            let last = ((center + radius).floor() as isize).min(src_len as isize - 1);
            let last = last.max(first as isize) as usize;

            let mut values: Vec<f32> = (first..=last)
                .map(|k| catmull_rom((k as f64 - center) / filter_scale) as f32)
                .collect();
            let sum: f32 = values.iter().sum();
            if sum.abs() > f32::EPSILON {
                for v in &mut values {
                    *v /= sum;
                }
            } else {
                // Degenerate tap set — fall back to nearest neighbour rather
                // than emitting a transparent black row.
                values.iter_mut().for_each(|v| *v = 0.0);
                let nearest = (center.round().clamp(first as f64, last as f64) as usize) - first;
                values[nearest] = 1.0;
            }
            Weights { first, values }
        })
        .collect()
}

/// Resamples one axis of a tightly packed RGBA8 buffer.
fn resample_axis(
    src: &[u8],
    src_width: usize,
    src_height: usize,
    dst_len: usize,
    axis: Axis,
) -> Vec<u8> {
    let (src_len, dst_width, dst_height) = match axis {
        Axis::Horizontal => (src_width, dst_len, src_height),
        Axis::Vertical => (src_height, src_width, dst_len),
    };
    let weights = weights_for(src_len, dst_len);
    let mut out = vec![0u8; dst_width * dst_height * 4];

    for y in 0..dst_height {
        for x in 0..dst_width {
            let w = match axis {
                Axis::Horizontal => &weights[x],
                Axis::Vertical => &weights[y],
            };
            let mut acc = [0f32; 4];
            for (i, weight) in w.values.iter().enumerate() {
                let (sx, sy) = match axis {
                    Axis::Horizontal => (w.first + i, y),
                    Axis::Vertical => (x, w.first + i),
                };
                let p = (sy * src_width + sx) * 4;
                for c in 0..4 {
                    acc[c] += f32::from(src[p + c]) * weight;
                }
            }
            let p = (y * dst_width + x) * 4;
            for c in 0..4 {
                // Catmull-Rom overshoots at high-contrast edges — exactly where
                // text lives — so clamping is mandatory, not defensive.
                out[p + c] = acc[c].round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}
