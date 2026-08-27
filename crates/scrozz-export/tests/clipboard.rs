//! Clipboard flavours (decision D10).
//!
//! None of these touch the real clipboard. The user's clipboard is *their*
//! clipboard, and a test suite that clobbers it while they are mid-copy is a
//! bug in the test suite. The one test that does write is opt-in through
//! `SCROZZ_TEST_CLIPBOARD=1` and is skipped otherwise.

mod common;

use common::{decode, pixel_at, rgba, solid};
use scrozz_core::{ColorSpace, PixelFormat};
use scrozz_export::{
    ClipboardPlatform, FlavourKind, ImageFormat, RgbaImage,
    clipboard::{arboard_delivers, bmp, dib_24, dib_v5, gaps, offer, preferred_kinds, tiff},
    profile_for, to_straight_rgba8,
};

const PLATFORMS: [ClipboardPlatform; 4] = [
    ClipboardPlatform::MacOs,
    ClipboardPlatform::Windows,
    ClipboardPlatform::X11,
    ClipboardPlatform::Wayland,
];

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn i32_at(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// Two rows: red on top, blue underneath. Row order bugs are then obvious.
fn two_tone() -> RgbaImage {
    let frame = rgba(2, 2, |_, y| {
        if y == 0 {
            [255, 0, 0, 255]
        } else {
            [0, 0, 255, 255]
        }
    });
    to_straight_rgba8(&frame).expect("normalises")
}

// ---------------------------------------------------------------------------
// What gets offered
// ---------------------------------------------------------------------------

#[test]
fn png_is_offered_first_on_every_platform() {
    // D10 requires PNG always be present, and both NSPasteboard and the Windows
    // clipboard read declaration order as a preference ranking — so the
    // lossless, smallest, most portable flavour has to lead.
    for platform in PLATFORMS {
        assert_eq!(
            preferred_kinds(platform).first(),
            Some(&FlavourKind::Png),
            "{platform:?} does not lead with PNG"
        );
    }
}

#[test]
fn each_platform_is_offered_more_than_one_flavour() {
    // The entire point of D10: offering only PNG means pasting silently fails
    // in exactly the older Office and chat clients people paste into most.
    for platform in PLATFORMS {
        assert!(
            preferred_kinds(platform).len() >= 2,
            "{platform:?} is only offered one flavour"
        );
    }
}

#[test]
fn the_platform_type_names_are_the_ones_the_platform_actually_uses() {
    let names = |p: ClipboardPlatform| {
        preferred_kinds(p)
            .iter()
            .map(|k| k.platform_type(p))
            .collect::<Vec<_>>()
    };

    assert_eq!(
        names(ClipboardPlatform::MacOs),
        ["public.png", "NSPasteboardTypeTIFF"]
    );
    assert_eq!(
        names(ClipboardPlatform::Windows),
        ["PNG", "CF_DIBV5", "CF_DIB"]
    );
    assert_eq!(
        names(ClipboardPlatform::X11),
        ["image/png", "image/bmp", "image/tiff"]
    );
    assert_eq!(
        names(ClipboardPlatform::Wayland),
        ["image/png", "image/bmp", "image/tiff"]
    );
}

#[test]
fn every_offered_flavour_carries_bytes_and_the_right_name() {
    let frame = solid(6, 4, [12, 200, 90]);
    for platform in PLATFORMS {
        let flavours = offer(&frame, platform).expect("builds flavours");
        assert_eq!(flavours.len(), preferred_kinds(platform).len());

        for flavour in &flavours {
            assert!(
                !flavour.bytes.is_empty(),
                "{:?} produced no bytes",
                flavour.kind
            );
            assert_eq!(flavour.platform_type, flavour.kind.platform_type(platform));
        }
    }
}

#[test]
fn the_png_flavour_is_a_real_png_of_the_real_picture() {
    let frame = rgba(5, 3, common::pattern);
    let flavours = offer(&frame, ClipboardPlatform::MacOs).expect("builds flavours");
    let png = &flavours[0];

    assert_eq!(ImageFormat::sniff(&png.bytes), Some(ImageFormat::Png));
    let (w, h, data) = decode(&png.bytes);
    assert_eq!((w, h), (5, 3));
    assert_eq!(pixel_at(&data, w, 3, 1), common::pattern(3, 1));
}

#[test]
fn a_premultiplied_capture_reaches_the_clipboard_with_straight_alpha() {
    // The same black-fringe bug, one layer further out: every flavour is built
    // from the normalised image, so none of them can reintroduce it.
    let frame = common::frame(
        2,
        2,
        8,
        PixelFormat::RgbaPremultiplied8,
        ColorSpace::Srgb,
        |_, _| [128, 0, 0, 128],
    );
    let flavours = offer(&frame, ClipboardPlatform::MacOs).expect("builds flavours");

    let (w, _, data) = decode(&flavours[0].bytes);
    let [r, _, _, a] = pixel_at(&data, w, 0, 0);
    assert!(
        r >= 250 && a == 128,
        "clipboard PNG kept premultiplied samples: {r}, {a}"
    );
}

// ---------------------------------------------------------------------------
// CF_DIBV5
// ---------------------------------------------------------------------------

#[test]
fn the_v5_dib_header_is_the_shape_windows_expects() {
    let image = two_tone();
    let dib = dib_v5(&image, None);

    assert_eq!(u32_at(&dib, 0), 124, "BITMAPV5HEADER is exactly 124 bytes");
    assert_eq!(i32_at(&dib, 4), 2, "width");
    assert_eq!(
        i32_at(&dib, 8),
        2,
        "height must be positive, meaning bottom-up rows"
    );
    assert_eq!(u16_at(&dib, 12), 1, "planes");
    assert_eq!(u16_at(&dib, 14), 32, "bits per pixel");
    assert_eq!(
        u32_at(&dib, 16),
        3,
        "BI_BITFIELDS, so the masks below are honoured"
    );
    assert_eq!(u32_at(&dib, 20), 16, "image byte count");
    assert_eq!(u32_at(&dib, 40), 0x00FF_0000, "red mask");
    assert_eq!(u32_at(&dib, 44), 0x0000_FF00, "green mask");
    assert_eq!(u32_at(&dib, 48), 0x0000_00FF, "blue mask");
    assert_eq!(u32_at(&dib, 52), 0xFF00_0000, "alpha mask");
    assert_eq!(dib.len(), 124 + 16);
}

#[test]
fn v5_dib_rows_run_bottom_up_and_samples_are_bgra() {
    let dib = dib_v5(&two_tone(), None);
    let pixels = &dib[124..];

    // The image's bottom row is blue, and it comes first.
    assert_eq!(
        &pixels[0..4],
        &[255, 0, 0, 255],
        "first stored row should be the blue one, in BGRA"
    );
    // The image's top row is red, and it comes last.
    assert_eq!(
        &pixels[8..12],
        &[0, 0, 255, 255],
        "last stored row should be the red one, in BGRA"
    );
}

#[test]
fn the_v5_dib_carries_a_wide_gamut_profile_that_arboard_cannot() {
    // The one thing this flavour does that handing `arboard` raw samples cannot:
    // a Display P3 capture keeps its colour through a paste.
    let profile = profile_for(ColorSpace::DisplayP3).expect("P3 has a profile");
    let image = two_tone();
    let dib = dib_v5(&image, Some(&profile));

    assert_eq!(
        u32_at(&dib, 56),
        0x4D42_4544,
        "bV5CSType should be PROFILE_EMBEDDED"
    );
    let offset = u32_at(&dib, 112) as usize;
    let size = u32_at(&dib, 116) as usize;
    assert_eq!(size, profile.len());
    assert_eq!(
        &dib[offset..offset + size],
        profile.as_slice(),
        "the profile offset is measured from the start of the header"
    );

    // Without a profile the header must say sRGB rather than point at nothing.
    let bare = dib_v5(&image, None);
    assert_eq!(u32_at(&bare, 56), 0x7352_4742, "LCS_sRGB");
    assert_eq!(u32_at(&bare, 112), 0);
    assert_eq!(u32_at(&bare, 116), 0);
}

// ---------------------------------------------------------------------------
// CF_DIB
// ---------------------------------------------------------------------------

#[test]
fn the_24_bit_dib_pads_every_row_to_four_bytes() {
    // Three pixels is nine bytes, which must become twelve. Forgetting this is
    // the diagonal-skew bug in its other habitat.
    let frame = solid(3, 2, [10, 20, 30]);
    let image = to_straight_rgba8(&frame).unwrap();
    let dib = dib_24(&image, [255, 255, 255]);

    assert_eq!(u32_at(&dib, 0), 40, "BITMAPINFOHEADER");
    assert_eq!(u16_at(&dib, 14), 24, "bits per pixel");
    assert_eq!(u32_at(&dib, 16), 0, "BI_RGB");
    assert_eq!(u32_at(&dib, 20), 24, "two rows of twelve padded bytes");
    assert_eq!(dib.len(), 40 + 24);
    assert_eq!(&dib[40..43], &[30, 20, 10], "samples are BGR");
    assert_eq!(
        &dib[49..52],
        &[0, 0, 0],
        "the padding must be zero, not image data"
    );
}

#[test]
fn the_24_bit_dib_composites_transparency_instead_of_dropping_it() {
    // Applications that ask for CF_DIB are the ones that ignore an alpha mask,
    // so a transparent capture handed to them as 32-bit data pastes as black
    // rectangles. Compositing over white is what they actually want.
    let frame = rgba(2, 1, |_, _| [255, 0, 0, 0]);
    let image = to_straight_rgba8(&frame).unwrap();
    let dib = dib_24(&image, [255, 255, 255]);

    assert_eq!(
        &dib[40..43],
        &[255, 255, 255],
        "fully transparent should become the background"
    );
}

// ---------------------------------------------------------------------------
// BMP
// ---------------------------------------------------------------------------

#[test]
fn the_bmp_file_header_points_at_the_pixels() {
    let image = two_tone();
    let file = bmp(&image, None);

    assert_eq!(&file[0..2], b"BM");
    assert_eq!(u32_at(&file, 2) as usize, file.len(), "declared file size");
    assert_eq!(
        u32_at(&file, 10),
        14 + 124,
        "pixel data follows the two headers"
    );
    assert_eq!(
        &file[14..],
        &dib_v5(&image, None)[..],
        "the body is the V5 DIB"
    );
}

// ---------------------------------------------------------------------------
// TIFF
// ---------------------------------------------------------------------------

/// Returns `(tag, kind, count, value)` for each directory entry.
fn tiff_entries(bytes: &[u8]) -> Vec<(u16, u16, u32, u32)> {
    assert_eq!(&bytes[0..2], b"II", "little-endian byte order mark");
    assert_eq!(u16_at(bytes, 2), 42, "the TIFF magic number");
    let ifd = u32_at(bytes, 4) as usize;
    let count = u16_at(bytes, ifd) as usize;

    (0..count)
        .map(|i| {
            let at = ifd + 2 + i * 12;
            (
                u16_at(bytes, at),
                u16_at(bytes, at + 2),
                u32_at(bytes, at + 4),
                u32_at(bytes, at + 8),
            )
        })
        .collect()
}

#[test]
fn the_tiff_directory_is_well_formed_and_in_ascending_tag_order() {
    // Readers are entitled to binary-search the directory, so an out-of-order
    // tag is not a cosmetic problem — it makes the tag invisible.
    let image = two_tone();
    let bytes = tiff(&image, None);
    let entries = tiff_entries(&bytes);

    let tags: Vec<u16> = entries.iter().map(|e| e.0).collect();
    assert!(
        tags.windows(2).all(|w| w[0] < w[1]),
        "tags out of order: {tags:?}"
    );
    assert_eq!(
        tags,
        [
            256, 257, 258, 259, 262, 273, 277, 278, 279, 282, 283, 296, 338
        ]
    );

    let value = |tag: u16| entries.iter().find(|e| e.0 == tag).expect("tag present").3;
    assert_eq!(value(256), 2, "ImageWidth");
    assert_eq!(value(257), 2, "ImageLength");
    assert_eq!(value(259), 1, "Compression: none");
    assert_eq!(value(262), 2, "PhotometricInterpretation: RGB");
    assert_eq!(value(277), 4, "SamplesPerPixel");
    assert_eq!(value(279), 16, "StripByteCounts");
    assert_eq!(
        value(338),
        2,
        "ExtraSamples: unassociated, which is TIFF for straight alpha"
    );
}

#[test]
fn the_tiff_pixel_strip_is_top_down_rgba_where_the_directory_says_it_is() {
    let image = two_tone();
    let bytes = tiff(&image, None);
    let entries = tiff_entries(&bytes);
    let offset = entries.iter().find(|e| e.0 == 273).unwrap().3 as usize;
    let length = entries.iter().find(|e| e.0 == 279).unwrap().3 as usize;

    assert_eq!(length, image.data.len());
    assert_eq!(&bytes[offset..offset + length], image.data.as_slice());
    // Unlike a DIB, TIFF's default is top-down, so red comes first here.
    assert_eq!(&bytes[offset..offset + 4], &[255, 0, 0, 255]);
}

#[test]
fn the_tiff_carries_its_profile_at_an_even_offset() {
    // An odd-length profile would otherwise misalign the strip that follows it,
    // and TIFF requires offsets be even.
    let profile = profile_for(ColorSpace::DisplayP3).expect("P3 has a profile");
    let image = two_tone();
    let bytes = tiff(&image, Some(&profile));
    let entries = tiff_entries(&bytes);

    let icc = entries
        .iter()
        .find(|e| e.0 == 34_675)
        .expect("the ICC tag is present");
    assert_eq!(icc.1, 7, "UNDEFINED");
    assert_eq!(icc.2 as usize, profile.len());
    assert_eq!(
        &bytes[icc.3 as usize..icc.3 as usize + profile.len()],
        profile.as_slice()
    );

    let strip = entries.iter().find(|e| e.0 == 273).unwrap().3;
    assert_eq!(strip % 2, 0, "the strip offset must be even");
    assert_eq!(
        &bytes[strip as usize..strip as usize + image.data.len()],
        image.data.as_slice()
    );
}

// ---------------------------------------------------------------------------
// The honest gap report
// ---------------------------------------------------------------------------

#[test]
fn every_platform_reports_at_least_one_thing_arboard_cannot_do() {
    // If this ever becomes empty it should be because the gap was closed, not
    // because the report quietly stopped being maintained.
    for platform in PLATFORMS {
        assert!(
            !gaps(platform).is_empty(),
            "{platform:?} claims a complete offer"
        );
    }
}

#[test]
fn nothing_is_reported_as_both_delivered_and_missing() {
    for platform in PLATFORMS {
        let delivered = arboard_delivers(platform);
        for gap in gaps(platform) {
            assert!(
                !delivered.contains(&gap.platform_type),
                "{platform:?} lists {} as both delivered and missing",
                gap.platform_type
            );
        }
    }
}

#[test]
fn delivered_and_missing_together_account_for_every_flavour_d10_asks_for() {
    // The report is only useful if it is exhaustive: a flavour that is neither
    // delivered nor listed as a gap is one nobody knows is absent.
    for platform in PLATFORMS {
        let delivered = arboard_delivers(platform);
        let missing: Vec<_> = gaps(platform).iter().map(|g| g.platform_type).collect();

        for kind in preferred_kinds(platform) {
            let name = kind.platform_type(platform);
            assert!(
                delivered.contains(&name) || missing.contains(&name),
                "{platform:?}: {name} is unaccounted for"
            );
        }
    }
}

#[test]
fn each_gap_explains_itself_and_says_what_would_fix_it() {
    for platform in PLATFORMS {
        for gap in gaps(platform) {
            assert!(
                gap.reason.len() > 40,
                "{platform:?}/{}: the reason should say what actually happens",
                gap.platform_type
            );
            assert!(
                gap.native_work.len() > 40,
                "{platform:?}/{}: a gap without a remedy is not actionable",
                gap.platform_type
            );
        }
    }
}

#[test]
fn the_macos_gap_is_the_missing_png_declaration() {
    let macos = gaps(ClipboardPlatform::MacOs);
    assert_eq!(macos.len(), 1);
    assert_eq!(macos[0].platform_type, "public.png");
    assert_eq!(
        arboard_delivers(ClipboardPlatform::MacOs),
        ["NSPasteboardTypeTIFF"]
    );
}

// ---------------------------------------------------------------------------
// The real clipboard, opt-in only
// ---------------------------------------------------------------------------

#[test]
fn writing_to_the_real_clipboard() {
    // Skipped unless SCROZZ_TEST_CLIPBOARD=1. Whoever is at this keyboard has
    // things on their clipboard, and replacing them to satisfy a test is not a
    // trade the test gets to make on their behalf. Text is saved and restored;
    // a clipboard holding an image cannot be put back, which is the other
    // reason this is opt-in.
    if std::env::var("SCROZZ_TEST_CLIPBOARD").as_deref() != Ok("1") {
        eprintln!("skipped: set SCROZZ_TEST_CLIPBOARD=1 to write to the real clipboard");
        return;
    }

    use scrozz_export::{Clipboard, SystemClipboard};

    let saved = arboard::Clipboard::new()
        .ok()
        .and_then(|mut c| c.get_text().ok());
    let frame = solid(8, 8, [20, 140, 255]);
    let result = SystemClipboard::new().write_image_with_report(&frame);

    if let Some(text) = saved
        && let Ok(mut c) = arboard::Clipboard::new()
    {
        let _ = c.set_text(text);
    }

    let report = result.expect("writes").expect("SystemClipboard reports");
    assert_eq!(report.platform, ClipboardPlatform::current());
    assert!(!report.delivered.is_empty());
}
