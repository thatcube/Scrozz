//! Encoder tests.
//!
//! Three of these exist because of three specific, visible bugs: a padded
//! stride skews the picture, premultiplied alpha fringes every soft edge with
//! black, and BGRA encoded as RGBA swaps red and blue. Each is checked against
//! a pattern that could not pass by accident.

mod common;

use common::{PADDING_SENTINEL, decode, embedded_profile, frame, pattern, pixel_at, rgba, solid};
use scrozz_core::{ColorSpace, Error, PixelFormat};
use scrozz_export::{
    ColorConversion, EncodeOptions, Encoder, FrameEncoder, ImageFormat, RgbaImage, convert_to_srgb,
    profile_for, to_straight_rgba8,
};

const LOSSLESS: [ImageFormat; 2] = [ImageFormat::Png, ImageFormat::WebP];
const ALL: [ImageFormat; 3] = [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP];

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

#[test]
fn lossless_formats_round_trip_every_pixel_exactly() {
    let source = rgba(23, 17, pattern);
    let encoder = FrameEncoder::new();

    for format in LOSSLESS {
        let bytes = encoder.encode(&source, format).expect("encodes");
        let (w, h, data) = decode(&bytes);
        assert_eq!((w, h), (23, 17), "{format:?} changed the dimensions");

        for y in 0..h {
            for x in 0..w {
                assert_eq!(
                    pixel_at(&data, w, x, y),
                    pattern(x, y),
                    "{format:?} altered the pixel at ({x}, {y})"
                );
            }
        }
    }
}

#[test]
fn jpeg_keeps_the_dimensions_and_stays_close_to_the_original() {
    let source = solid(16, 9, [200, 40, 90]);
    let bytes = FrameEncoder::new()
        .encode(&source, ImageFormat::Jpeg)
        .expect("encodes");
    let (w, h, data) = decode(&bytes);

    assert_eq!((w, h), (16, 9));
    let [r, g, b, a] = pixel_at(&data, w, 8, 4);
    assert_eq!(
        a, 255,
        "JPEG has no alpha channel, so it must decode as opaque"
    );
    for (got, want) in [(r, 200), (g, 40), (b, 90)] {
        assert!(
            i32::from(got).abs_diff(want) <= 4,
            "JPEG at quality {} drifted too far: {got} vs {want}",
            EncodeOptions::default().jpeg_quality
        );
    }
}

#[test]
fn many_shapes_round_trip_through_png() {
    // A stand-in for a property test: assorted sizes and paddings, every one of
    // them a fresh chance for an off-by-one in the stride arithmetic.
    let mut rng = common::Rng::new(0xC0FFEE);
    let encoder = FrameEncoder::new();

    for _ in 0..40 {
        let w = rng.range(1, 40);
        let h = rng.range(1, 40);
        let pad = rng.range(0, 32) as usize;
        let source = frame(w, h, pad, PixelFormat::Rgba8, ColorSpace::Srgb, pattern);

        let bytes = encoder.encode(&source, ImageFormat::Png).expect("encodes");
        let (dw, dh, data) = decode(&bytes);
        assert_eq!((dw, dh), (w, h), "{w}x{h} pad {pad}: wrong dimensions");
        for y in 0..h {
            for x in 0..w {
                assert_eq!(
                    pixel_at(&data, dw, x, y),
                    pattern(x, y),
                    "{w}x{h} pad {pad}: wrong pixel at ({x}, {y})"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stride
// ---------------------------------------------------------------------------

#[test]
fn a_padded_stride_does_not_skew_the_image() {
    // Vertical stripes are the pattern a skew destroys most obviously: if the
    // padding is treated as image data, each successive row starts `pad` bytes
    // late and the stripes lean over.
    let width = 13;
    let height = 11;
    let pad = 37; // Not a multiple of 4 either, so a sloppy fix is caught too.
    let stripe = |x: u32| {
        if x.is_multiple_of(2) {
            [255, 0, 0, 255]
        } else {
            [0, 0, 255, 255]
        }
    };

    let source = frame(
        width,
        height,
        pad,
        PixelFormat::Rgba8,
        ColorSpace::Srgb,
        |x, _| stripe(x),
    );
    let bytes = FrameEncoder::new()
        .encode(&source, ImageFormat::Png)
        .expect("encodes");
    let (w, h, data) = decode(&bytes);

    assert_eq!((w, h), (width, height));
    for y in 0..h {
        for x in 0..w {
            assert_eq!(
                pixel_at(&data, w, x, y),
                stripe(x),
                "row {y} is offset: the stride padding leaked into the image"
            );
        }
    }
}

#[test]
fn stride_padding_bytes_never_reach_the_output() {
    let source = frame(8, 8, 16, PixelFormat::Rgba8, ColorSpace::Srgb, |_, _| {
        [10, 20, 30, 255]
    });
    let image = to_straight_rgba8(&source).expect("normalises");

    assert_eq!(
        image.data.len(),
        8 * 8 * 4,
        "the buffer should be tightly packed"
    );
    assert!(
        !image.data.contains(&PADDING_SENTINEL),
        "a padding byte survived into the normalised image"
    );
}

#[test]
fn a_short_buffer_is_an_error_rather_than_a_panic() {
    let mut source = solid(10, 10, [1, 2, 3]);
    source.data.truncate(10);
    assert!(
        FrameEncoder::new()
            .encode(&source, ImageFormat::Png)
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// Premultiplied alpha
// ---------------------------------------------------------------------------

#[test]
fn premultiplied_alpha_does_not_leave_a_black_fringe() {
    // A pure red pixel at half opacity is stored premultiplied as (128, 0, 0).
    // Encoded without un-premultiplying, it decodes as *dark* red — which is
    // exactly the black halo seen around rounded window corners. Straight alpha
    // must restore the full-strength red.
    let source = frame(
        4,
        4,
        0,
        PixelFormat::RgbaPremultiplied8,
        ColorSpace::Srgb,
        |_, _| [128, 0, 0, 128],
    );

    for format in LOSSLESS {
        let bytes = FrameEncoder::new()
            .encode(&source, format)
            .expect("encodes");
        let (w, _, data) = decode(&bytes);
        let [r, g, b, a] = pixel_at(&data, w, 1, 1);

        assert_eq!(a, 128, "{format:?} changed the alpha");
        assert!(
            r >= 250,
            "{format:?} kept the premultiplied red ({r}); every soft edge would be \
             fringed with black"
        );
        assert_eq!((g, b), (0, 0), "{format:?} disturbed the other channels");
    }
}

#[test]
fn a_soft_edge_keeps_its_colour_all_the_way_to_full_transparency() {
    // The realistic version of the fringe: a horizontal alpha ramp over a solid
    // colour, which is what an anti-aliased window corner is. Every step must
    // decode to the same colour, differing only in alpha.
    let width = 32;
    let source = frame(
        width,
        1,
        0,
        PixelFormat::RgbaPremultiplied8,
        ColorSpace::Srgb,
        |x, _| {
            let a = (x * 255 / (width - 1)) as u8;
            let scale = |c: u32| ((c * u32::from(a) + 127) / 255) as u8;
            [scale(20), scale(140), scale(255), a]
        },
    );

    let bytes = FrameEncoder::new()
        .encode(&source, ImageFormat::Png)
        .expect("encodes");
    let (w, _, data) = decode(&bytes);

    for x in 1..w {
        let [r, g, b, a] = pixel_at(&data, w, x, 0);
        // Rounding in an 8-bit premultiply is lossy at low alpha — that is
        // physics, not a bug — so the tolerance scales with how little signal
        // survived. What must never happen is the colour trending towards zero.
        let tolerance = (255 / u32::from(a).max(1) + 2) as u8;
        for (got, want, name) in [(r, 20, "red"), (g, 140, "green"), (b, 255, "blue")] {
            assert!(
                got.abs_diff(want) <= tolerance,
                "at alpha {a} the {name} channel decoded as {got}, not ~{want}: the edge \
                 is being darkened towards black"
            );
        }
    }
}

#[test]
fn a_fully_transparent_pixel_does_not_divide_by_zero() {
    let source = frame(
        2,
        2,
        0,
        PixelFormat::RgbaPremultiplied8,
        ColorSpace::Srgb,
        |_, _| [0, 0, 0, 0],
    );
    let bytes = FrameEncoder::new()
        .encode(&source, ImageFormat::Png)
        .expect("encodes");
    let (w, _, data) = decode(&bytes);
    assert_eq!(pixel_at(&data, w, 0, 0), [0, 0, 0, 0]);
}

// ---------------------------------------------------------------------------
// Channel order
// ---------------------------------------------------------------------------

#[test]
fn bgra_is_not_encoded_with_red_and_blue_swapped() {
    // Stored BGRA, so these bytes mean blue=10, green=20, red=200.
    let source = frame(5, 5, 0, PixelFormat::Bgra8, ColorSpace::Srgb, |_, _| {
        [10, 20, 200, 255]
    });

    for format in LOSSLESS {
        let bytes = FrameEncoder::new()
            .encode(&source, format)
            .expect("encodes");
        let (w, _, data) = decode(&bytes);
        assert_eq!(
            pixel_at(&data, w, 2, 2),
            [200, 20, 10, 255],
            "{format:?} did not swap the blue and red channels"
        );
    }
}

#[test]
fn bgra_and_rgba_describing_the_same_picture_encode_identically() {
    let as_rgba = rgba(9, 6, pattern);
    let as_bgra = frame(9, 6, 12, PixelFormat::Bgra8, ColorSpace::Srgb, |x, y| {
        let [r, g, b, a] = pattern(x, y);
        [b, g, r, a]
    });

    let encoder = FrameEncoder::new();
    assert_eq!(
        encoder.encode(&as_rgba, ImageFormat::Png).unwrap(),
        encoder.encode(&as_bgra, ImageFormat::Png).unwrap(),
        "the same picture in two layouts must produce the same file"
    );
}

// ---------------------------------------------------------------------------
// Colour management
// ---------------------------------------------------------------------------

#[test]
fn known_wide_gamut_vectors_are_converted_to_srgb_pixels() {
    let image = RgbaImage {
        width: 1,
        height: 1,
        data: vec![128, 64, 32, 73],
    };

    assert_eq!(
        convert_to_srgb(&image, ColorSpace::DisplayP3).unwrap().data,
        [138, 59, 21, 73]
    );
    assert_eq!(
        convert_to_srgb(&image, ColorSpace::Rec2020).unwrap().data,
        [167, 67, 39, 73]
    );
}

#[test]
fn encoder_srgb_conversion_changes_pixels_profile_and_preserves_alpha() {
    let source = frame(
        1,
        1,
        0,
        PixelFormat::Rgba8,
        ColorSpace::DisplayP3,
        |_, _| [128, 64, 32, 73],
    );
    let bytes = FrameEncoder::with_options(EncodeOptions {
        color_conversion: ColorConversion::ToSrgb,
        drop_opaque_alpha: false,
        ..EncodeOptions::default()
    })
    .encode(&source, ImageFormat::Png)
    .unwrap();

    let (width, _, data) = decode(&bytes);
    assert_eq!(pixel_at(&data, width, 0, 0), [138, 59, 21, 73]);
    assert_eq!(embedded_profile(&bytes), profile_for(ColorSpace::Srgb));
}

#[test]
fn converting_unknown_samples_is_refused_instead_of_retagged() {
    let source = frame(1, 1, 0, PixelFormat::Rgba8, ColorSpace::Unknown, |_, _| {
        [1, 2, 3, 255]
    });
    let error = FrameEncoder::with_options(EncodeOptions {
        color_conversion: ColorConversion::ToSrgb,
        ..EncodeOptions::default()
    })
    .encode(&source, ImageFormat::Png)
    .unwrap_err();

    assert!(matches!(error, Error::InvalidRequest(_)));
}

#[test]
fn every_format_carries_a_display_p3_profile() {
    let source = frame(
        4,
        4,
        0,
        PixelFormat::Rgba8,
        ColorSpace::DisplayP3,
        |_, _| [255, 0, 0, 255],
    );
    let expected = profile_for(ColorSpace::DisplayP3).expect("P3 has a profile");

    for format in ALL {
        let bytes = FrameEncoder::new()
            .encode(&source, format)
            .expect("encodes");
        let embedded = embedded_profile(&bytes)
            .unwrap_or_else(|| panic!("{format:?} dropped the Display P3 profile"));
        assert_eq!(
            embedded, expected,
            "{format:?} altered the profile; a wide-gamut capture would decode wrong"
        );
    }
}

#[test]
fn rec2020_is_carried_too() {
    let source = frame(4, 4, 0, PixelFormat::Rgba8, ColorSpace::Rec2020, |_, _| {
        [0, 255, 0, 255]
    });
    let bytes = FrameEncoder::new()
        .encode(&source, ImageFormat::Png)
        .expect("encodes");
    assert_eq!(embedded_profile(&bytes), profile_for(ColorSpace::Rec2020));
}

#[test]
fn an_unknown_colour_space_embeds_nothing_rather_than_claiming_srgb() {
    // The whole point of ColorSpace::Unknown: a viewer acts on an embedded
    // profile, so guessing sRGB is worse than saying nothing.
    let source = frame(4, 4, 0, PixelFormat::Rgba8, ColorSpace::Unknown, |_, _| {
        [1, 2, 3, 255]
    });

    assert_eq!(profile_for(ColorSpace::Unknown), None);
    for format in ALL {
        let bytes = FrameEncoder::new()
            .encode(&source, format)
            .expect("encodes");
        assert_eq!(
            embedded_profile(&bytes),
            None,
            "{format:?} invented a profile for an unknown colour space"
        );
    }
}

#[test]
fn the_srgb_profile_is_embedded_by_default_and_can_be_switched_off() {
    let source = solid(4, 4, [128, 128, 128]);

    let tagged = FrameEncoder::new()
        .encode(&source, ImageFormat::Png)
        .expect("encodes");
    assert_eq!(embedded_profile(&tagged), profile_for(ColorSpace::Srgb));

    let bare = FrameEncoder::with_options(EncodeOptions {
        embed_srgb_profile: false,
        ..EncodeOptions::default()
    })
    .encode(&source, ImageFormat::Png)
    .expect("encodes");
    assert_eq!(embedded_profile(&bare), None);
    assert!(
        bare.len() < tagged.len(),
        "dropping the profile should shrink the file"
    );
}

#[test]
fn the_profiles_for_different_spaces_actually_differ() {
    let srgb = profile_for(ColorSpace::Srgb).unwrap();
    let p3 = profile_for(ColorSpace::DisplayP3).unwrap();
    let rec2020 = profile_for(ColorSpace::Rec2020).unwrap();

    assert_ne!(srgb, p3);
    assert_ne!(p3, rec2020);
    assert_ne!(srgb, rec2020);
    for profile in [&srgb, &p3, &rec2020] {
        assert_eq!(
            u32::from_be_bytes(profile[0..4].try_into().unwrap()) as usize,
            profile.len(),
            "the declared profile size must match the bytes"
        );
        assert_eq!(&profile[36..40], b"acsp", "missing the ICC signature");
    }
}

// ---------------------------------------------------------------------------
// Alpha handling and format identification
// ---------------------------------------------------------------------------

#[test]
fn an_opaque_capture_is_stored_without_a_useless_alpha_channel() {
    let source = solid(64, 64, [90, 120, 200]);
    let with_drop = FrameEncoder::new()
        .encode(&source, ImageFormat::Png)
        .unwrap();
    let without = FrameEncoder::with_options(EncodeOptions {
        drop_opaque_alpha: false,
        ..EncodeOptions::default()
    })
    .encode(&source, ImageFormat::Png)
    .unwrap();

    assert!(
        with_drop.len() < without.len(),
        "an opaque screenshot should not pay for an alpha channel"
    );
    let (_, _, data) = decode(&with_drop);
    assert!(data.chunks(4).all(|px| px[3] == 255));
}

#[test]
fn jpeg_composites_transparency_over_the_configured_background() {
    let source = rgba(4, 4, |_, _| [0, 0, 0, 0]);
    let encoder = FrameEncoder::with_options(EncodeOptions {
        jpeg_background: [0, 0, 255],
        jpeg_quality: 100,
        ..EncodeOptions::default()
    });

    let bytes = encoder.encode(&source, ImageFormat::Jpeg).expect("encodes");
    let (w, _, data) = decode(&bytes);
    let [r, g, b, _] = pixel_at(&data, w, 2, 2);
    assert!(
        b > 200 && r < 40 && g < 40,
        "fully transparent pixels should become the background colour, got \
         ({r}, {g}, {b})"
    );
}

#[test]
fn what_the_encoder_produces_is_what_sniffing_reports() {
    let source = solid(8, 8, [7, 7, 7]);
    for format in ALL {
        let bytes = FrameEncoder::new()
            .encode(&source, format)
            .expect("encodes");
        assert_eq!(ImageFormat::sniff(&bytes), Some(format));
    }
    assert_eq!(ImageFormat::sniff(b"not an image at all"), None);
    assert_eq!(ImageFormat::sniff(&[]), None);
    assert_eq!(
        ImageFormat::sniff(b"RIFF1234"),
        None,
        "truncated RIFF must not match"
    );
}

#[test]
fn extensions_and_media_types_line_up() {
    assert_eq!(ImageFormat::Png.extension(), "png");
    assert_eq!(ImageFormat::Jpeg.extension(), "jpg");
    assert_eq!(ImageFormat::WebP.extension(), "webp");
    assert_eq!(ImageFormat::Jpeg.media_type(), "image/jpeg");
    assert!(!ImageFormat::Jpeg.supports_alpha());
    assert!(ImageFormat::Png.supports_alpha() && ImageFormat::WebP.supports_alpha());
}
