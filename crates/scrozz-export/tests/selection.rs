//! Destination-aware automatic encoding policy.

mod common;

use common::{frame, rgba, solid};
use scrozz_core::{ColorSpace, PixelFormat};
use scrozz_export::{
    ColorConversion, ContentKind, DestinationCapabilities, DestinationColorSpace, DestinationKind,
    DestinationProfile, ImageFormat, PngEffort, select_export,
};

#[test]
fn clipboard_always_selects_fast_png() {
    let mut profile = DestinationProfile::clipboard();
    profile.capabilities = DestinationCapabilities::new([ImageFormat::Jpeg]);

    let selection = select_export(&solid(2, 2, [1, 2, 3]), &profile, ContentKind::Photographic)
        .expect("clipboard has a mandatory PNG representation");

    assert_eq!(selection.format, ImageFormat::Png);
    assert_eq!(selection.options.png_effort, PngEffort::Fast);
}

#[test]
fn screenshots_default_to_lossless_formats_supported_by_the_destination() {
    let folder = select_export(
        &solid(2, 2, [1, 2, 3]),
        &DestinationProfile::folder(),
        ContentKind::Screenshot,
    )
    .unwrap();
    assert_eq!(folder.format, ImageFormat::Png);
    assert_eq!(folder.options.png_effort, PngEffort::Maximum);

    let upload = select_export(
        &solid(2, 2, [1, 2, 3]),
        &DestinationProfile::upload(DestinationCapabilities::new([
            ImageFormat::Jpeg,
            ImageFormat::WebP,
        ])),
        ContentKind::Screenshot,
    )
    .unwrap();
    assert_eq!(upload.format, ImageFormat::WebP);
    assert_eq!(upload.options.color_conversion, ColorConversion::ToSrgb);
}

#[test]
fn opaque_photographs_may_select_jpeg_when_the_caller_marks_them() {
    let selection = select_export(
        &solid(2, 2, [80, 90, 100]),
        &DestinationProfile::folder(),
        ContentKind::Photographic,
    )
    .unwrap();
    assert_eq!(selection.format, ImageFormat::Jpeg);
}

#[test]
fn any_transparency_prevents_jpeg_selection() {
    let transparent = rgba(2, 1, |x, _| {
        if x == 0 {
            [1, 2, 3, 255]
        } else {
            [4, 5, 6, 254]
        }
    });
    let profile = DestinationProfile::upload(DestinationCapabilities::new([
        ImageFormat::Jpeg,
        ImageFormat::Png,
    ]));
    assert_eq!(
        select_export(&transparent, &profile, ContentKind::Photographic)
            .unwrap()
            .format,
        ImageFormat::Png
    );

    let jpeg_only = DestinationProfile::upload(DestinationCapabilities::new([ImageFormat::Jpeg]));
    assert!(select_export(&transparent, &jpeg_only, ContentKind::Photographic).is_err());
}

#[test]
fn destination_colour_policy_is_explicit() {
    let mut profile = DestinationProfile {
        kind: DestinationKind::Upload,
        capabilities: DestinationCapabilities::new([ImageFormat::Png]),
        color_space: DestinationColorSpace::Preserve,
    };
    let source = frame(
        1,
        1,
        0,
        PixelFormat::Rgba8,
        ColorSpace::DisplayP3,
        |_, _| [1, 2, 3, 255],
    );
    assert_eq!(
        select_export(&source, &profile, ContentKind::Screenshot)
            .unwrap()
            .options
            .color_conversion,
        ColorConversion::Preserve
    );
    profile.color_space = DestinationColorSpace::Srgb;
    assert_eq!(
        select_export(&source, &profile, ContentKind::Screenshot)
            .unwrap()
            .options
            .color_conversion,
        ColorConversion::ToSrgb
    );
}
