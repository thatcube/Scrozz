//! Frame normalisation, resampling and the upscale decision.
//!
//! All platform-independent. The upscale decision in particular is the single
//! biggest quality lever for screenshots — a 1× capture of 11pt UI text is
//! genuinely below what any recogniser handles well — so it is worth pinning
//! precisely rather than trusting it to look right on the developer's Retina
//! display, where the factor is always 1.

use scrozz_core::{ColorSpace, Frame, PhysicalSize, PixelFormat, ScaleFactor};
use scrozz_ocr::UpscalePolicy;
use scrozz_ocr::prepare::{Rgba8Image, prepare, upscale_factor};

/// Builds a frame whose rows are padded, because real capture buffers are.
fn frame(
    width: u32,
    height: u32,
    format: PixelFormat,
    scale: f64,
    padding: usize,
    fill: [u8; 4],
) -> Frame {
    let stride = width as usize * 4 + padding;
    let mut data = vec![0u8; stride * height as usize];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let at = y * stride + x * 4;
            data[at..at + 4].copy_from_slice(&fill);
        }
        // Poison the padding: anything that reads it will produce garbage.
        for byte in data[y * stride + width as usize * 4..(y + 1) * stride].iter_mut() {
            *byte = 0xAB;
        }
    }
    Frame {
        data,
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride,
        format,
        color_space: ColorSpace::Srgb,
        scale: ScaleFactor::new(scale),
    }
}

#[test]
fn stride_padding_is_dropped() {
    let f = frame(4, 3, PixelFormat::Rgba8, 1.0, 17, [10, 20, 30, 255]);
    let image = Rgba8Image::from_frame(&f).expect("well-formed frame");
    assert_eq!(image.width, 4);
    assert_eq!(image.height, 3);
    assert_eq!(image.data.len(), 4 * 3 * 4);
    assert!(
        image
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [10, 20, 30, 255]),
        "padding bytes leaked into the image"
    );
}

#[test]
fn bgra_is_swizzled_to_rgba() {
    // Stored B=30, G=20, R=10 must read back as R=10, G=20, B=30.
    let f = frame(2, 2, PixelFormat::Bgra8, 1.0, 0, [30, 20, 10, 255]);
    let image = Rgba8Image::from_frame(&f).expect("well-formed frame");
    assert_eq!(&image.data[..4], &[10, 20, 30, 255]);
}

#[test]
fn premultiplied_alpha_is_undone() {
    // Half-transparent white is stored as 128,128,128,128; straight alpha is
    // 255,255,255,128. Getting this wrong makes light-on-dark UI text mushy.
    let f = frame(
        1,
        1,
        PixelFormat::RgbaPremultiplied8,
        1.0,
        0,
        [128, 128, 128, 128],
    );
    let image = Rgba8Image::from_frame(&f).expect("well-formed frame");
    assert_eq!(image.data[3], 128);
    for channel in &image.data[..3] {
        assert!(
            (*channel as i32 - 255).abs() <= 1,
            "expected ~255, got {channel}"
        );
    }
}

#[test]
fn bgra_premultiplied_is_both_swizzled_and_unpremultiplied() {
    // The variant Windows.Graphics.Capture actually produces. Half-transparent
    // white arrives as B=128,G=128,R=128,A=128 and must come back as straight
    // R=255,G=255,B=255,A=128. Handling only one of the two transforms is the
    // easy mistake, and either half alone silently corrupts every WGC capture.
    let f = frame(
        1,
        1,
        PixelFormat::BgraPremultiplied8,
        1.0,
        0,
        [64, 128, 192, 128],
    );
    let image = Rgba8Image::from_frame(&f).expect("well-formed frame");
    assert_eq!(
        image.data[3], 128,
        "alpha must survive un-premultiplication"
    );
    // Stored BGR 64,128,192 is RGB 192,128,64 premultiplied by 0.5, so straight
    // alpha doubles each back to 255 (clamped), 255, 128.
    assert_eq!(image.data[0], 255, "R: 192/0.5 clamps to 255");
    assert_eq!(image.data[1], 255, "G: 128/0.5 clamps to 255");
    assert!(
        (i32::from(image.data[2]) - 128).abs() <= 1,
        "B: 64/0.5 is ~128, got {}",
        image.data[2]
    );
}

#[test]
fn opaque_bgra_premultiplied_only_swizzles() {
    let f = frame(
        1,
        1,
        PixelFormat::BgraPremultiplied8,
        1.0,
        0,
        [30, 20, 10, 255],
    );
    let image = Rgba8Image::from_frame(&f).expect("well-formed frame");
    assert_eq!(&image.data[..], &[10, 20, 30, 255]);
}

#[test]
fn fully_transparent_pixels_do_not_divide_by_zero() {
    let f = frame(1, 1, PixelFormat::RgbaPremultiplied8, 1.0, 0, [0, 0, 0, 0]);
    let image = Rgba8Image::from_frame(&f).expect("well-formed frame");
    assert_eq!(&image.data[..], &[0, 0, 0, 0]);
}

#[test]
fn opaque_premultiplied_pixels_are_untouched() {
    let f = frame(
        1,
        1,
        PixelFormat::RgbaPremultiplied8,
        1.0,
        0,
        [77, 88, 99, 255],
    );
    let image = Rgba8Image::from_frame(&f).expect("well-formed frame");
    assert_eq!(&image.data[..], &[77, 88, 99, 255]);
}

#[test]
fn a_short_buffer_is_rejected_rather_than_panicking() {
    let mut f = frame(4, 4, PixelFormat::Rgba8, 1.0, 0, [0, 0, 0, 255]);
    f.data.truncate(8);
    let err = Rgba8Image::from_frame(&f).expect_err("short buffer must be refused");
    assert!(
        matches!(err, scrozz_core::Error::InvalidRequest(_)),
        "got {err:?}"
    );
}

#[test]
fn a_zero_sized_frame_is_rejected() {
    let f = frame(0, 0, PixelFormat::Rgba8, 1.0, 0, [0, 0, 0, 255]);
    let err = Rgba8Image::from_frame(&f).expect_err("empty frame must be refused");
    assert!(
        matches!(err, scrozz_core::Error::InvalidRequest(_)),
        "got {err:?}"
    );
}

/// A 1× capture is the case that matters. 2× effective resolution is the
/// target, so the factor must be at least 2.
#[test]
fn one_x_captures_are_upscaled() {
    let factor = upscale_factor(
        1200,
        800,
        ScaleFactor::new(1.0),
        UpscalePolicy::Automatic,
        None,
    );
    assert!(factor >= 2.0, "1x capture must be upscaled, got {factor}");
}

/// On Retina the pixels are already there. Upscaling again costs time and adds
/// resampling artefacts for nothing.
#[test]
fn two_x_captures_are_left_alone() {
    let factor = upscale_factor(
        2400,
        1600,
        ScaleFactor::new(2.0),
        UpscalePolicy::Automatic,
        None,
    );
    assert_eq!(factor, 1.0);
}

/// A small crop — a tooltip, a single label — needs more than the scale rule
/// gives it, because absolute size matters to a recogniser, not just density.
#[test]
fn small_crops_get_extra_help() {
    let by_scale = upscale_factor(
        2000,
        1400,
        ScaleFactor::new(2.0),
        UpscalePolicy::Automatic,
        None,
    );
    let tiny = upscale_factor(
        200,
        60,
        ScaleFactor::new(2.0),
        UpscalePolicy::Automatic,
        None,
    );
    assert_eq!(by_scale, 1.0);
    assert!(
        tiny > 1.0,
        "a 200x60 crop needs upscaling even at 2x, got {tiny}"
    );
}

#[test]
fn the_factor_is_a_whole_number() {
    for (w, h, scale) in [(1200u32, 800u32, 1.0), (300, 120, 1.0), (640, 480, 1.5)] {
        let factor = upscale_factor(
            w,
            h,
            ScaleFactor::new(scale),
            UpscalePolicy::Automatic,
            None,
        );
        assert_eq!(
            factor.fract(),
            0.0,
            "{w}x{h}@{scale} gave {factor}; fractional factors move the resampler off pixel centres"
        );
    }
}

#[test]
fn policy_off_disables_upscaling_entirely() {
    assert_eq!(
        upscale_factor(100, 40, ScaleFactor::new(1.0), UpscalePolicy::Off, None),
        1.0
    );
}

#[test]
fn a_fixed_policy_is_still_clamped() {
    let factor = upscale_factor(
        100,
        100,
        ScaleFactor::new(1.0),
        UpscalePolicy::Fixed(99),
        None,
    );
    assert!(
        (1.0..=4.0).contains(&factor),
        "an absurd fixed factor must be clamped, got {factor}"
    );
}

#[test]
fn fixed_one_means_no_resampling() {
    assert_eq!(
        upscale_factor(
            100,
            40,
            ScaleFactor::new(1.0),
            UpscalePolicy::Fixed(1),
            None
        ),
        1.0
    );
}

/// A 6K display capture upscaled 2× would be 80 megapixels. The budget holds
/// the *enlargement* back — but it must not shrink a capture the user already
/// has, which would throw away real detail to satisfy a memory rule.
#[test]
fn huge_frames_are_not_upscaled_into_oblivion() {
    let factor = upscale_factor(
        6016,
        3384,
        ScaleFactor::new(1.0),
        UpscalePolicy::Automatic,
        None,
    );
    assert_eq!(
        factor, 1.0,
        "a 20MP capture must be passed through, not resampled"
    );
}

#[test]
fn the_budget_caps_enlargement_of_middling_frames() {
    // 3000x2000 = 6MP; 2x would be 24MP, over the 16MP budget.
    let factor = upscale_factor(
        3000,
        2000,
        ScaleFactor::new(1.0),
        UpscalePolicy::Automatic,
        None,
    );
    let pixels = 3000.0 * factor * 2000.0 * factor;
    assert!(factor >= 1.0, "the budget must never shrink, got {factor}");
    assert!(pixels <= 16_000_001.0, "{factor}x gives {pixels} pixels");
}

/// Windows' OCR engine rejects images past `MaxImageDimension`. Rather than
/// erroring, shrink — a downscaled read beats no read.
#[test]
fn a_max_dimension_can_push_the_factor_below_one() {
    let factor = upscale_factor(
        4000,
        2000,
        ScaleFactor::new(1.0),
        UpscalePolicy::Automatic,
        Some(1000),
    );
    assert!(factor < 1.0, "must shrink to fit 1000px, got {factor}");
    assert!(4000.0 * factor <= 1000.0);
}

#[test]
fn a_generous_max_dimension_does_not_interfere() {
    let unbounded = upscale_factor(
        1200,
        800,
        ScaleFactor::new(1.0),
        UpscalePolicy::Automatic,
        None,
    );
    let bounded = upscale_factor(
        1200,
        800,
        ScaleFactor::new(1.0),
        UpscalePolicy::Automatic,
        Some(100_000),
    );
    assert_eq!(unbounded, bounded);
}

#[test]
fn preparing_a_retina_frame_is_a_passthrough() {
    let f = frame(1000, 800, PixelFormat::Bgra8, 2.0, 12, [1, 2, 3, 255]);
    let prepared = prepare(&f, UpscalePolicy::Automatic, None).expect("prepare");
    assert_eq!(prepared.upscale, 1.0);
    assert_eq!(prepared.image.width, 1000);
    assert_eq!(prepared.image.height, 800);
    assert_eq!(prepared.source_size, f.size);
}

#[test]
fn preparing_a_one_x_frame_grows_it_by_the_reported_factor() {
    let f = frame(400, 300, PixelFormat::Rgba8, 1.0, 0, [200, 200, 200, 255]);
    let prepared = prepare(&f, UpscalePolicy::Automatic, None).expect("prepare");
    assert!(prepared.upscale > 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let expected_w = (400.0 * prepared.upscale).round() as u32;
    assert_eq!(prepared.image.width, expected_w);
    assert_eq!(
        prepared.image.data.len(),
        (prepared.image.width * prepared.image.height * 4) as usize
    );
}

/// Resampling a flat colour must give back that colour. Catmull-Rom overshoots
/// at edges; if weights are not normalised, flat regions drift or clip.
#[test]
fn resampling_a_flat_colour_preserves_it() {
    let data = (0..64 * 48 * 4)
        .map(|i| match i % 4 {
            0 => 173,
            1 => 91,
            2 => 42,
            _ => 255,
        })
        .collect();
    let image = Rgba8Image::new(64, 48, data).expect("valid image");

    let up = image.resample(192, 144);
    assert_eq!(up.width, 192);
    for pixel in up.data.as_chunks::<4>().0.iter() {
        assert_eq!(
            *pixel,
            [173u8, 91, 42, 255],
            "flat colour drifted under upscale"
        );
    }

    let down = up.resample(32, 24);
    for pixel in down.data.as_chunks::<4>().0.iter() {
        assert_eq!(
            *pixel,
            [173u8, 91, 42, 255],
            "flat colour drifted under downscale"
        );
    }
}

#[test]
fn resampling_to_the_same_size_is_identity() {
    let mut data = vec![0u8; 8 * 8 * 4];
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = if i % 4 == 3 { 255 } else { (i % 251) as u8 };
    }
    let image = Rgba8Image::new(8, 8, data.clone()).expect("valid image");
    let same = image.resample(8, 8);
    assert_eq!(same.data, data);
}

/// Nearest-neighbour would alias a checkerboard into a solid colour or moiré;
/// a windowed filter that widens on downscale averages it to mid-grey.
#[test]
fn downscaling_averages_rather_than_point_sampling() {
    let (w, h) = (64u32, 64u32);
    let mut data = vec![255u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let at = ((y * w + x) * 4) as usize;
            let value = if (x + y) % 2 == 0 { 0 } else { 255 };
            data[at] = value;
            data[at + 1] = value;
            data[at + 2] = value;
        }
    }
    let image = Rgba8Image::new(w, h, data).expect("valid image");
    let small = image.resample(8, 8);
    for pixel in small.data.as_chunks::<4>().0.iter() {
        assert!(
            (100..=155).contains(&pixel[0]),
            "checkerboard should average to mid-grey, got {}",
            pixel[0]
        );
    }
}

#[test]
fn resampling_to_zero_yields_an_empty_image_rather_than_panicking() {
    let image = Rgba8Image::new(4, 4, vec![255; 64]).expect("valid image");
    assert_eq!(image.resample(0, 4).data.len(), 0);
    assert_eq!(image.resample(4, 0).width, 0);
}

#[test]
fn constructing_an_image_validates_its_buffer() {
    assert!(Rgba8Image::new(4, 4, vec![0; 63]).is_err());
    assert!(Rgba8Image::new(0, 4, vec![]).is_err());
    assert!(Rgba8Image::new(4, 4, vec![0; 64]).is_ok());
}
