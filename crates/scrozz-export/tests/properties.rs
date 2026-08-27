//! Property tests for the encoders.
//!
//! The unit and integration tests pin down specific pixels; these sweep many
//! shapes and check *invariants*. The interesting ones are the two independence
//! properties — padding must not change the output, and channel order must not
//! change the output — because those are exactly the mistakes that produce a
//! skewed or blue-tinted screenshot, and a single hand-picked example is easy
//! to accidentally choose so that the bug cancels out.
//!
//! The generator is a fixed-seed LCG rather than `proptest`: this crate is not
//! allowed to add dependencies, and a fixed seed means a failure reported by CI
//! reproduces exactly from the test name alone.

mod common;

use common::{Rng, decode, frame, pixel_at};
use scrozz_core::{ColorSpace, ColorSpace::*, Frame, PixelFormat, PixelFormat::*};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, profile_for};

/// How many random cases each property runs.
///
/// Large enough that a systematic error in any code path is hit many times
/// over, small enough that the whole file still runs in well under a second.
const CASES: usize = 200;

const FORMATS: [ImageFormat; 3] = [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP];
const PIXEL_FORMATS: [PixelFormat; 3] = [Rgba8, Bgra8, RgbaPremultiplied8];
const SPACES: [ColorSpace; 4] = [Srgb, DisplayP3, Rec2020, Unknown];

/// The straight-alpha RGBA the encoders are expected to produce, computed the
/// slow obvious way.
///
/// Deliberately a second implementation: per pixel, indexed rather than
/// chunked, and in floating point rather than fixed point. If it agrees with
/// the real conversion across hundreds of random frames, the two are unlikely
/// to be wrong in the same direction.
fn reference(frame: &Frame) -> Vec<u8> {
    let (width, height) = (frame.width() as usize, frame.height() as usize);
    let mut out = Vec::with_capacity(width * height * 4);

    for y in 0..height {
        for x in 0..width {
            let i = y * frame.stride + x * 4;
            let px = [
                frame.data[i],
                frame.data[i + 1],
                frame.data[i + 2],
                frame.data[i + 3],
            ];
            out.extend_from_slice(&match frame.format {
                Rgba8 => px,
                Bgra8 => [px[2], px[1], px[0], px[3]],
                RgbaPremultiplied8 => {
                    let a = px[3];
                    if a == 0 {
                        [0, 0, 0, 0]
                    } else {
                        let straight =
                            |c: u8| (f64::from(c) * 255.0 / f64::from(a)).round().min(255.0) as u8;
                        [straight(px[0]), straight(px[1]), straight(px[2]), a]
                    }
                }
            });
        }
    }
    out
}

/// A random frame, with padding, format and colour space drawn independently.
fn arbitrary(rng: &mut Rng) -> Frame {
    let width = rng.range(1, 23);
    let height = rng.range(1, 23);
    let pad = rng.range(0, 3) as usize * 4;
    let format = PIXEL_FORMATS[rng.range(0, 2) as usize];
    let space = SPACES[rng.range(0, 3) as usize];

    // Premultiplied buffers are generated in-gamut (c <= a) because that is what
    // a compositor produces; the out-of-gamut clamp has its own unit test.
    let mut samples = Vec::with_capacity((width * height) as usize);
    for _ in 0..width * height {
        let a = rng.byte();
        let cap = if format == RgbaPremultiplied8 { a } else { 255 };
        samples.push([
            rng.byte().min(cap),
            rng.byte().min(cap),
            rng.byte().min(cap),
            a,
        ]);
    }

    frame(width, height, pad, format, space, |x, y| {
        samples[(y * width + x) as usize]
    })
}

#[test]
fn lossless_formats_return_exactly_the_pixels_that_went_in() {
    let mut rng = Rng::new(0x5C20_2202);

    for case in 0..CASES {
        let source = arbitrary(&mut rng);
        let expected = reference(&source);

        for format in [ImageFormat::Png, ImageFormat::WebP] {
            let bytes = FrameEncoder::new()
                .encode(&source, format)
                .expect("encodes");
            let (width, height, actual) = decode(&bytes);

            assert_eq!(
                (width, height),
                (source.width(), source.height()),
                "case {case}"
            );
            assert_eq!(
                actual,
                expected,
                "case {case}: {format:?} changed the pixels of a {}x{} {:?} frame with \
                 {} bytes of padding",
                source.width(),
                source.height(),
                source.format,
                source.stride - source.width() as usize * 4
            );
        }
    }
}

#[test]
fn padding_never_changes_the_result() {
    // The skew bug in one property: a frame is the same picture whether or not
    // its rows are padded, so the encoded bytes must be identical too. An
    // encoder that ignores stride fails on the first padded case.
    let mut rng = Rng::new(0x9E37_79B9);

    for case in 0..CASES {
        let width = rng.range(1, 17);
        let height = rng.range(1, 17);
        let format = PIXEL_FORMATS[rng.range(0, 2) as usize];
        let mut samples = Vec::new();
        for _ in 0..width * height {
            let a = rng.byte();
            let cap = if format == RgbaPremultiplied8 { a } else { 255 };
            samples.push([
                rng.byte().min(cap),
                rng.byte().min(cap),
                rng.byte().min(cap),
                a,
            ]);
        }
        let sample = |x: u32, y: u32| samples[(y * width + x) as usize];

        let tight = frame(width, height, 0, format, Srgb, sample);
        let padded = frame(width, height, 4 + (case % 4) * 4, format, Srgb, sample);
        assert!(
            padded.stride > tight.stride,
            "the padded frame must actually be padded"
        );

        for format in FORMATS {
            let a = FrameEncoder::new().encode(&tight, format).expect("encodes");
            let b = FrameEncoder::new()
                .encode(&padded, format)
                .expect("encodes");
            assert_eq!(
                a, b,
                "case {case}: padding leaked into the {format:?} output"
            );
        }
    }
}

#[test]
fn channel_order_is_a_property_of_the_buffer_not_of_the_picture() {
    // A BGRA frame and the RGBA frame showing the same picture must encode
    // identically. A missing swap turns every red screenshot blue.
    let mut rng = Rng::new(0xB6EA_7A11);

    for case in 0..CASES {
        let width = rng.range(1, 17);
        let height = rng.range(1, 17);
        let mut samples = Vec::new();
        for _ in 0..width * height {
            samples.push([rng.byte(), rng.byte(), rng.byte(), rng.byte()]);
        }

        let as_rgba = frame(width, height, 0, Rgba8, Srgb, |x, y| {
            samples[(y * width + x) as usize]
        });
        let as_bgra = frame(width, height, 8, Bgra8, Srgb, |x, y| {
            let [r, g, b, a] = samples[(y * width + x) as usize];
            [b, g, r, a]
        });

        for format in FORMATS {
            let a = FrameEncoder::new()
                .encode(&as_rgba, format)
                .expect("encodes");
            let b = FrameEncoder::new()
                .encode(&as_bgra, format)
                .expect("encodes");
            assert_eq!(
                a, b,
                "case {case}: BGRA and RGBA of the same picture disagreed for {format:?}"
            );
        }
    }
}

#[test]
fn unpremultiplying_never_darkens_a_pixel() {
    // The black-fringing property. Un-premultiplying divides by alpha, so every
    // channel can only get lighter or stay put. If any channel comes out darker
    // than it went in, the division was skipped — which is precisely the halo
    // around a rounded window corner.
    let mut rng = Rng::new(0x00FF_1CE5);

    for case in 0..CASES {
        let width = rng.range(1, 13);
        let height = rng.range(1, 13);
        let mut samples = Vec::new();
        for _ in 0..width * height {
            let a = rng.byte();
            samples.push([rng.byte().min(a), rng.byte().min(a), rng.byte().min(a), a]);
        }

        let source = frame(width, height, 12, RgbaPremultiplied8, Srgb, |x, y| {
            samples[(y * width + x) as usize]
        });
        let bytes = FrameEncoder::new()
            .encode(&source, ImageFormat::Png)
            .expect("encodes");
        let (w, _, decoded) = decode(&bytes);

        for y in 0..height {
            for x in 0..width {
                let [pr, pg, pb, pa] = samples[(y * width + x) as usize];
                let out = pixel_at(&decoded, w, x, y);
                assert_eq!(out[3], pa, "case {case}: alpha must survive untouched");

                if pa == 0 {
                    continue;
                }
                for (channel, (was, now)) in [(pr, out[0]), (pg, out[1]), (pb, out[2])]
                    .into_iter()
                    .enumerate()
                {
                    assert!(
                        now >= was,
                        "case {case}: channel {channel} at ({x},{y}) darkened from {was} to \
                         {now} at alpha {pa} — this is the black fringe"
                    );
                    // And it must be recoverable: multiplying back by alpha
                    // returns where it started, give or take rounding.
                    let back = (u32::from(now) * u32::from(pa) + 127) / 255;
                    assert!(
                        back.abs_diff(u32::from(was)) <= 1,
                        "case {case}: channel {channel} at ({x},{y}) does not survive a \
                         round trip: {was} -> {now} -> {back}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_profile_is_embedded_exactly_when_the_colour_space_is_known() {
    // Never lying with sRGB is the whole point: an untagged file is treated as
    // sRGB by most viewers, so tagging an unknown capture as sRGB and leaving it
    // untagged look identical in practice — but tagging a *Display P3* capture
    // as sRGB does not, and that is the mistake this guards.
    let mut rng = Rng::new(0x1CC0_1CC0);

    for case in 0..CASES {
        let source = arbitrary(&mut rng);
        let expected = profile_for(source.color_space);
        assert_eq!(
            expected.is_none(),
            source.color_space == Unknown,
            "only Unknown may be profile-less"
        );

        for format in FORMATS {
            let bytes = FrameEncoder::new()
                .encode(&source, format)
                .expect("encodes");
            assert_eq!(
                common::embedded_profile(&bytes),
                expected,
                "case {case}: {format:?} lost or invented a profile for {:?}",
                source.color_space
            );
        }
    }
}

#[test]
fn every_encoded_image_identifies_itself() {
    // The folder destination picks the file extension by sniffing the bytes, so
    // a format that does not round-trip through `sniff` would be saved under the
    // wrong extension and fail to open by double-click.
    let mut rng = Rng::new(0x5A1F_F000);

    for case in 0..CASES / 4 {
        let source = arbitrary(&mut rng);
        for format in FORMATS {
            let bytes = FrameEncoder::new()
                .encode(&source, format)
                .expect("encodes");
            assert_eq!(ImageFormat::sniff(&bytes), Some(format), "case {case}");
        }
    }
}

#[test]
fn jpeg_keeps_the_shape_and_roughly_the_colour() {
    // JPEG cannot be checked pixel-for-pixel, but it can be held to the two
    // things that actually break: geometry and gross colour error. A skew or a
    // channel swap moves the mean error far beyond what quantisation explains.
    let mut rng = Rng::new(0x1A1A_7EC5);

    for case in 0..CASES / 4 {
        let width = rng.range(8, 32);
        let height = rng.range(8, 32);
        let source = frame(width, height, 4, Rgba8, Srgb, |x, y| {
            [(x * 8 % 256) as u8, (y * 8 % 256) as u8, 128, 255]
        });

        let bytes = FrameEncoder::new()
            .encode(&source, ImageFormat::Jpeg)
            .expect("encodes");
        let (w, h, decoded) = decode(&bytes);
        assert_eq!((w, h), (width, height), "case {case}");

        let mut total = 0u64;
        for y in 0..height {
            for x in 0..width {
                let out = pixel_at(&decoded, w, x, y);
                assert_eq!(out[3], 255, "case {case}: JPEG has no alpha to report");
                let want = [(x * 8 % 256) as u8, (y * 8 % 256) as u8, 128];
                for c in 0..3 {
                    total += u64::from(out[c].abs_diff(want[c]));
                }
            }
        }
        let mean = total as f64 / f64::from(width * height * 3);
        assert!(
            mean < 12.0,
            "case {case}: mean channel error {mean:.1} is not quantisation"
        );
    }
}
