//! Turning a capture buffer into something an encoder will accept.
//!
//! Every encoder in this crate wants the same thing: tightly packed, straight
//! alpha, red-green-blue-alpha byte order. A [`Frame`] is frequently none of
//! those. This module is the single place that difference is resolved, because
//! each of the three conversions has a characteristic visual failure and they
//! are much easier to reason about — and to test — in one place than scattered
//! through three codec paths:
//!
//! - **Stride.** [`Frame::stride`] usually exceeds `width * 4`, because capture
//!   APIs pad rows to an alignment boundary. Feeding the raw buffer to an
//!   encoder that assumes tight rows shifts every row by a constant amount and
//!   produces the classic diagonally-skewed image.
//! - **Premultiplied alpha.** macOS hands back
//!   [`PixelFormat::RgbaPremultiplied8`]. PNG and WebP store straight alpha, so
//!   writing premultiplied samples into them darkens every partially
//!   transparent pixel in proportion to its transparency — seen as a black
//!   fringe around rounded window corners and drop shadows.
//! - **Channel order.** Windows DXGI and several X11 paths hand back
//!   [`PixelFormat::Bgra8`]. Encoding that as RGBA swaps red and blue, which is
//!   not subtle.

use scrozz_core::{ColorSpace, Error, Frame, PixelFormat, Result};

/// A tightly packed, straight-alpha, RGBA8 image.
///
/// The invariant `data.len() == width * height * 4` holds by construction, which
/// matters because [`image`]'s encoders assert on it and would otherwise panic
/// rather than return an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `height` rows of `width * 4` bytes, red-green-blue-alpha, straight alpha.
    pub data: Vec<u8>,
}

impl RgbaImage {
    /// The pixel at `(x, y)`, or `None` if out of bounds.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }

        let i = (y as usize * self.width as usize + x as usize) * 4;
        Some([
            self.data[i],
            self.data[i + 1],
            self.data[i + 2],
            self.data[i + 3],
        ])
    }

    /// Whether every pixel is fully opaque.
    ///
    /// Worth knowing: an opaque screenshot — which is the overwhelmingly common
    /// case — encodes about a quarter smaller without its useless alpha channel.
    #[must_use]
    pub fn is_opaque(&self) -> bool {
        self.data.as_chunks::<4>().0.iter().all(|px| px[3] == 255)
    }

    /// Drops alpha, compositing over `background`.
    ///
    /// JPEG has no alpha channel, so something must be chosen. Discarding alpha
    /// without compositing would reveal whatever colour happened to sit in the
    /// unused channels of transparent pixels, which for premultiplied sources is
    /// black.
    #[must_use]
    pub fn to_rgb8(&self, background: [u8; 3]) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.width as usize * self.height as usize * 3);
        for px in self.data.as_chunks::<4>().0 {
            let a = u32::from(px[3]);
            if a == 255 {
                out.extend_from_slice(&px[..3]);
            } else {
                let inv = 255 - a;
                for c in 0..3 {
                    let blended =
                        (u32::from(px[c]) * a + u32::from(background[c]) * inv + 127) / 255;
                    out.push(blended.min(255) as u8);
                }
            }
        }
        out
    }
}

/// Converts encoded RGB samples from `source` to sRGB, preserving alpha.
///
/// Display P3 uses the sRGB transfer function; Rec. 2020 uses the Rec. 709
/// transfer function for the SDR samples represented by [`ColorSpace::Rec2020`].
/// Conversion linearises the source, transforms through CIE XYZ (D65), then
/// applies the sRGB transfer function. Out-of-gamut values are clipped only
/// after the matrix transform.
///
/// [`ColorSpace::Unknown`] is refused. Treating unknown samples as sRGB would
/// merely retag them, which is precisely the colour error this operation exists
/// to avoid. Callers that want pass-through behaviour should not request a
/// conversion.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] when `source` is unknown.
pub fn convert_to_srgb(image: &RgbaImage, source: ColorSpace) -> Result<RgbaImage> {
    convert_color_space(image, source, ColorSpace::Srgb)
}

/// Converts encoded RGB samples between supported colour spaces.
///
/// Alpha is preserved. Unknown colour spaces are refused unless source and
/// destination are both unknown, because assigning a transform without known
/// primaries would be a silent retagging error.
pub fn convert_color_space(
    image: &RgbaImage,
    source: ColorSpace,
    destination: ColorSpace,
) -> Result<RgbaImage> {
    if source == destination {
        return Ok(image.clone());
    }
    if source == ColorSpace::Unknown || destination == ColorSpace::Unknown {
        return Err(Error::InvalidRequest(
            "cannot convert an image whose source or destination colour space is unknown".into(),
        ));
    }

    let to_xyz = match source {
        ColorSpace::Srgb => SRGB_TO_XYZ,
        ColorSpace::DisplayP3 => DISPLAY_P3_TO_XYZ,
        ColorSpace::Rec2020 => REC2020_TO_XYZ,
        ColorSpace::Unknown => unreachable!("handled above"),
    };
    let decode = match source {
        ColorSpace::Srgb | ColorSpace::DisplayP3 => srgb_to_linear,
        ColorSpace::Rec2020 => rec2020_to_linear,
        ColorSpace::Unknown => unreachable!("handled above"),
    };
    let from_xyz = match destination {
        ColorSpace::Srgb => XYZ_TO_SRGB,
        ColorSpace::DisplayP3 => XYZ_TO_DISPLAY_P3,
        ColorSpace::Rec2020 => XYZ_TO_REC2020,
        ColorSpace::Unknown => unreachable!("handled above"),
    };
    let encode = match destination {
        ColorSpace::Srgb | ColorSpace::DisplayP3 => linear_to_srgb_u8,
        ColorSpace::Rec2020 => linear_to_rec2020_u8,
        ColorSpace::Unknown => unreachable!("handled above"),
    };

    let mut data = Vec::with_capacity(image.data.len());
    for pixel in image.data.as_chunks::<4>().0 {
        data.extend_from_slice(&convert_pixel(*pixel, to_xyz, from_xyz, decode, encode));
    }

    Ok(RgbaImage {
        width: image.width,
        height: image.height,
        data,
    })
}

/// Converts one straight-alpha sRGB authoring colour to `destination`.
pub fn convert_srgb_color(color: [u8; 4], destination: ColorSpace) -> Result<[u8; 4]> {
    if destination == ColorSpace::Unknown {
        return Ok(color);
    }
    if destination == ColorSpace::Srgb {
        return Ok(color);
    }
    let from_xyz = match destination {
        ColorSpace::DisplayP3 => XYZ_TO_DISPLAY_P3,
        ColorSpace::Rec2020 => XYZ_TO_REC2020,
        ColorSpace::Srgb | ColorSpace::Unknown => unreachable!("handled above"),
    };
    let encode = match destination {
        ColorSpace::DisplayP3 => linear_to_srgb_u8,
        ColorSpace::Rec2020 => linear_to_rec2020_u8,
        ColorSpace::Srgb | ColorSpace::Unknown => unreachable!("handled above"),
    };
    Ok(convert_pixel(
        color,
        SRGB_TO_XYZ,
        from_xyz,
        srgb_to_linear,
        encode,
    ))
}

type Matrix = [[f64; 3]; 3];

const SRGB_TO_XYZ: &Matrix = &[
    [
        0.412_390_799_265_959_5,
        0.357_584_339_383_877_96,
        0.180_480_788_401_834_3,
    ],
    [
        0.212_639_005_871_510_36,
        0.715_168_678_767_755_9,
        0.072_192_315_360_733_71,
    ],
    [
        0.019_330_818_715_591_85,
        0.119_194_779_794_625_99,
        0.950_532_152_249_660_7,
    ],
];

const DISPLAY_P3_TO_XYZ: &Matrix = &[
    [
        0.486_570_948_648_216_2,
        0.265_667_693_169_093_06,
        0.198_217_285_234_362_5,
    ],
    [
        0.228_974_564_069_748_8,
        0.691_738_521_836_506_4,
        0.079_286_914_093_745,
    ],
    [0.0, 0.045_113_381_858_902_64, 1.043_944_368_900_976],
];

const REC2020_TO_XYZ: &Matrix = &[
    [
        0.636_958_048_301_291_4,
        0.144_616_903_586_208_32,
        0.168_880_975_164_172_1,
    ],
    [
        0.262_700_212_011_267_1,
        0.677_998_071_518_870_8,
        0.059_301_716_469_861_96,
    ],
    [0.0, 0.028_072_693_049_087_428, 1.060_985_057_710_791],
];

const XYZ_TO_SRGB: &Matrix = &[
    [
        3.240_969_941_904_522_6,
        -1.537_383_177_570_094,
        -0.498_610_760_293_003_4,
    ],
    [
        -0.969_243_636_280_879_6,
        1.875_967_501_507_720_2,
        0.041_555_057_407_175_59,
    ],
    [
        0.055_630_079_696_993_66,
        -0.203_976_958_888_976_52,
        1.056_971_514_242_878_6,
    ],
];

const XYZ_TO_DISPLAY_P3: &Matrix = &[
    [
        2.493_496_911_941_425,
        -0.931_383_617_919_123_9,
        -0.402_710_784_450_716_84,
    ],
    [
        -0.829_488_969_561_574_7,
        1.762_664_060_318_346_3,
        0.023_624_685_841_943_577,
    ],
    [
        0.035_845_830_243_784_47,
        -0.076_172_389_268_041_82,
        0.956_884_524_007_687_2,
    ],
];

const XYZ_TO_REC2020: &Matrix = &[
    [
        1.716_651_187_971_268,
        -0.355_670_783_776_392,
        -0.253_366_281_373_66,
    ],
    [
        -0.666_684_351_832_489,
        1.616_481_236_634_939,
        0.015_768_545_813_911,
    ],
    [
        0.017_639_857_445_311,
        -0.042_770_613_257_809,
        0.942_103_121_235_474,
    ],
];

fn transform(matrix: &Matrix, value: [f64; 3]) -> [f64; 3] {
    matrix.map(|row| row[0] * value[0] + row[1] * value[1] + row[2] * value[2])
}

fn convert_pixel(
    pixel: [u8; 4],
    to_xyz: &Matrix,
    from_xyz: &Matrix,
    decode: fn(f64) -> f64,
    encode: fn(f64) -> u8,
) -> [u8; 4] {
    let linear = [
        decode(f64::from(pixel[0]) / 255.0),
        decode(f64::from(pixel[1]) / 255.0),
        decode(f64::from(pixel[2]) / 255.0),
    ];
    let xyz = transform(to_xyz, linear);
    let converted = transform(from_xyz, xyz);
    [
        encode(converted[0]),
        encode(converted[1]),
        encode(converted[2]),
        pixel[3],
    ]
}

fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn rec2020_to_linear(value: f64) -> f64 {
    if value < 0.081 {
        value / 4.5
    } else {
        ((value + 0.099) / 1.099).powf(1.0 / 0.45)
    }
}

fn linear_to_srgb_u8(linear: f64) -> u8 {
    let encoded = if linear <= 0.003_130_8 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (encoded.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn linear_to_rec2020_u8(linear: f64) -> u8 {
    let encoded = if linear < 0.018 {
        4.5 * linear
    } else {
        1.099 * linear.powf(0.45) - 0.099
    };
    (encoded.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Normalises a frame into tightly packed straight-alpha RGBA8.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] if the frame is empty or its buffer is too
/// short for its declared geometry. Both are caller bugs, and catching them here
/// turns what would otherwise be a panic inside a codec into a diagnosable
/// error at the boundary that produced it.
pub fn to_straight_rgba8(frame: &Frame) -> Result<RgbaImage> {
    let (width, height) = (frame.width(), frame.height());
    if width == 0 || height == 0 {
        return Err(Error::InvalidRequest(format!(
            "cannot encode a {width}x{height} frame: an image must have area"
        )));
    }
    if !frame.is_well_formed() {
        let needed = frame.stride.checked_mul(height as usize).map_or_else(
            || "an unaddressable number of bytes".to_owned(),
            |n| n.to_string(),
        );
        return Err(Error::InvalidRequest(format!(
            "frame buffer is {} bytes but {width}x{height} at stride {} needs {needed}",
            frame.data.len(),
            frame.stride,
        )));
    }

    let row_bytes = width as usize * frame.format.bytes_per_pixel();
    let mut data = Vec::with_capacity(row_bytes * height as usize);

    for y in 0..height as usize {
        let start = y * frame.stride;
        let row = &frame.data[start..start + row_bytes];
        match frame.format {
            PixelFormat::Rgba8 => data.extend_from_slice(row),
            PixelFormat::Bgra8 => {
                for px in row.as_chunks::<4>().0 {
                    data.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
            }
            PixelFormat::RgbaPremultiplied8 => {
                for px in row.as_chunks::<4>().0 {
                    let [r, g, b] = unpremultiply([px[0], px[1], px[2]], px[3]);
                    data.extend_from_slice(&[r, g, b, px[3]]);
                }
            }
            // What Windows.Graphics.Capture produces. Un-premultiply and swap
            // channel order in the same pass, rather than making the caller do
            // two full-image walks.
            PixelFormat::BgraPremultiplied8 => {
                for px in row.as_chunks::<4>().0 {
                    let [b, g, r] = unpremultiply([px[0], px[1], px[2]], px[3]);
                    data.extend_from_slice(&[r, g, b, px[3]]);
                }
            }
        }
    }

    Ok(RgbaImage {
        width,
        height,
        data,
    })
}

/// Recovers straight-alpha colour from premultiplied colour.
///
/// Fully transparent pixels carry no recoverable colour — `c = 0 * alpha` for
/// every `c` — so they become transparent black rather than an arbitrary
/// division result.
///
/// Channels are clamped because premultiplied buffers routinely contain
/// `c > a` by a unit or two from the compositor's own rounding, and an
/// unclamped divide would wrap.
fn unpremultiply(colour: [u8; 3], alpha: u8) -> [u8; 3] {
    if alpha == 0 {
        return [0, 0, 0];
    }
    if alpha == 255 {
        return colour;
    }
    let a = u32::from(alpha);
    colour.map(|c| (((u32::from(c) * 255) + a / 2) / a).min(255) as u8)
}

#[cfg(test)]
mod tests {
    use scrozz_core::{PhysicalSize, ScaleFactor};

    use super::*;

    #[test]
    fn transparent_pixels_lose_their_colour_rather_than_dividing_by_zero() {
        assert_eq!(unpremultiply([9, 9, 9], 0), [0, 0, 0]);
    }

    #[test]
    fn out_of_range_premultiplied_channels_clamp_instead_of_wrapping() {
        // c > a is out of gamut but compositors produce it; 200/128 * 255 > 255.
        assert_eq!(unpremultiply([200, 200, 200], 128), [255, 255, 255]);
    }

    #[test]
    fn half_alpha_white_round_trips_exactly() {
        assert_eq!(unpremultiply([128, 128, 128], 128), [255, 255, 255]);
    }

    #[test]
    fn hostile_stride_overflow_returns_an_error() {
        let frame = Frame {
            data: Vec::new(),
            size: PhysicalSize::new(1.0, 2.0),
            stride: usize::MAX,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::IDENTITY,
        };
        assert!(to_straight_rgba8(&frame).is_err());
    }

    #[test]
    fn colour_space_round_trip_preserves_srgb_samples_with_small_quantisation_error() {
        let source = RgbaImage {
            width: 3,
            height: 1,
            data: vec![210, 30, 70, 255, 20, 190, 110, 180, 32, 80, 230, 96],
        };
        let p3 = convert_color_space(&source, ColorSpace::Srgb, ColorSpace::DisplayP3).unwrap();
        let round_trip = convert_color_space(&p3, ColorSpace::DisplayP3, ColorSpace::Srgb).unwrap();
        for (actual, expected) in round_trip.data.iter().zip(&source.data) {
            assert!(
                actual.abs_diff(*expected) <= 2,
                "{actual} differs from {expected}"
            );
        }
    }

    #[test]
    fn single_colour_conversion_preserves_alpha() {
        let converted = convert_srgb_color([230, 40, 120, 77], ColorSpace::Rec2020).unwrap();
        assert_eq!(converted[3], 77);
        assert_ne!(converted[..3], [230, 40, 120]);
    }

    #[test]
    fn unknown_colour_space_is_never_guessed() {
        let image = RgbaImage {
            width: 1,
            height: 1,
            data: vec![1, 2, 3, 255],
        };
        assert!(convert_color_space(&image, ColorSpace::Unknown, ColorSpace::Srgb).is_err());
        assert_eq!(
            convert_color_space(&image, ColorSpace::Unknown, ColorSpace::Unknown).unwrap(),
            image
        );
    }
}
