//! Putting a capture on the clipboard in every flavour the platform can offer.
//!
//! # Why one format is not enough (decision D10)
//!
//! Pasting is the app's most-used action, and it fails *silently*: an app that
//! does not understand the offered format simply does nothing, and the user
//! concludes the capture did not work. The formats different applications
//! accept diverge sharply — modern editors take PNG, older Office builds and a
//! number of chat clients take only the platform's native bitmap — so the fix is
//! to put several representations on the clipboard at once and let the receiving
//! application choose. All three operating systems support exactly this.
//!
//! # What this module actually delivers today
//!
//! `arboard` offers **one** image representation per platform. That is a real
//! shortfall against D10, and rather than quietly offering less, this module:
//!
//! 1. builds the bytes for every flavour the platform should be offered —
//!    [`Flavour`], produced by [`offer`] — all in safe Rust with no new
//!    dependencies, so a native shim would be a thin layer over tested code;
//! 2. writes what `arboard` supports; and
//! 3. reports precisely what is missing and what native work would close it —
//!    [`ClipboardReport`] and [`gaps`].
//!
//! The gap analysis is data rather than prose so it can be asserted on in tests
//! and surfaced in diagnostics, and so it cannot rot silently.

use std::borrow::Cow;

use scrozz_core::{ColorSpace, Error, Frame, Result};

use crate::{
    Clipboard, ImageFormat,
    encode::{EncodeOptions, FrameEncoder, PngEffort},
    icc::profile_for,
    pixels::{RgbaImage, to_straight_rgba8},
};

/// The clipboard implementation in play, which is not simply the OS.
///
/// X11 and Wayland are separate because they are separate protocols with
/// separate ownership models, and `arboard` implements them separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardPlatform {
    /// macOS `NSPasteboard`.
    MacOs,
    /// Windows clipboard.
    Windows,
    /// X11 selections.
    X11,
    /// Wayland `wl_data_device`.
    Wayland,
}

impl ClipboardPlatform {
    /// The platform this build targets.
    ///
    /// X11 is assumed on Unix because it is what `arboard` falls back to and
    /// because XWayland makes it work under a Wayland session anyway. The
    /// distinction only affects the advisory gap report, never correctness.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::X11
        }
    }
}

/// A representation of the capture, in one encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlavourKind {
    /// PNG. Always present, per D10.
    Png,
    /// Baseline uncompressed TIFF, the native macOS pasteboard image type.
    Tiff,
    /// A `BITMAPV5HEADER` device-independent bitmap with an alpha channel.
    DibV5,
    /// A 24-bit `BITMAPINFOHEADER` DIB, for applications that ignore alpha.
    Dib,
    /// A `.bmp` file: a DIB with a file header on the front.
    Bmp,
}

impl FlavourKind {
    /// The name this flavour is registered under on `platform`.
    #[must_use]
    pub const fn platform_type(self, platform: ClipboardPlatform) -> &'static str {
        match (self, platform) {
            (Self::Png, ClipboardPlatform::MacOs) => "public.png",
            (Self::Png, ClipboardPlatform::Windows) => "PNG",
            (Self::Png, _) => "image/png",
            (Self::Tiff, ClipboardPlatform::MacOs) => "NSPasteboardTypeTIFF",
            (Self::Tiff, _) => "image/tiff",
            (Self::DibV5, _) => "CF_DIBV5",
            (Self::Dib, _) => "CF_DIB",
            (Self::Bmp, _) => "image/bmp",
        }
    }
}

/// One representation, ready to hand to the platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flavour {
    /// What encoding this is.
    pub kind: FlavourKind,
    /// The name the platform knows it by.
    pub platform_type: &'static str,
    /// The encoded bytes.
    pub bytes: Vec<u8>,
}

/// The flavours a platform should be offered, most-preferred first.
///
/// Order is not cosmetic. Both `NSPasteboard` and the Windows clipboard treat
/// declaration order as a preference ranking, so a receiving application that
/// understands several picks the first — which must be PNG, the smallest of
/// these and the only lossless one that also survives a round trip through the
/// web.
#[must_use]
pub const fn preferred_kinds(platform: ClipboardPlatform) -> &'static [FlavourKind] {
    match platform {
        // TIFF is what every native macOS app has accepted since 1989 and is
        // what `NSImage` itself writes.
        ClipboardPlatform::MacOs => &[FlavourKind::Png, FlavourKind::Tiff],
        // CF_DIBV5 carries alpha; CF_DIB is the flat 24-bit form that the
        // applications most likely to fail on PNG will actually take.
        ClipboardPlatform::Windows => &[FlavourKind::Png, FlavourKind::DibV5, FlavourKind::Dib],
        // Toolkits disagree here: GTK offers image/png and image/bmp, Qt adds
        // image/tiff, and several older apps request only image/bmp.
        ClipboardPlatform::X11 | ClipboardPlatform::Wayland => {
            &[FlavourKind::Png, FlavourKind::Bmp, FlavourKind::Tiff]
        }
    }
}

/// Builds every flavour `platform` should be offered for `frame`.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] for a malformed frame, or [`Error::Codec`]
/// if PNG encoding failed.
pub fn offer(frame: &Frame, platform: ClipboardPlatform) -> Result<Vec<Flavour>> {
    let image = to_straight_rgba8(frame)?;
    let profile = profile_for(frame.color_space);
    // Clipboard bytes are consumed immediately and never stored, so spending
    // seconds on maximum PNG compression would only add latency to a paste.
    let encoder = FrameEncoder::with_options(EncodeOptions {
        png_effort: PngEffort::Fast,
        ..EncodeOptions::default()
    });

    preferred_kinds(platform)
        .iter()
        .map(|&kind| {
            let bytes = match kind {
                FlavourKind::Png => {
                    encoder.encode_rgba(&image, frame.color_space, ImageFormat::Png)?
                }
                FlavourKind::Tiff => tiff(&image, profile.as_deref()),
                FlavourKind::DibV5 => dib_v5(&image, profile.as_deref()),
                FlavourKind::Dib => dib_24(&image, [255, 255, 255]),
                FlavourKind::Bmp => bmp(&image, profile.as_deref()),
            };
            Ok(Flavour {
                kind,
                platform_type: kind.platform_type(platform),
                bytes,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Gap analysis
// ---------------------------------------------------------------------------

/// A flavour D10 calls for that this build does not actually put on the
/// clipboard, and what it would take to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlavourGap {
    /// The platform type that is missing.
    pub platform_type: &'static str,
    /// Why `arboard` does not offer it.
    pub reason: &'static str,
    /// The native work that would close the gap.
    pub native_work: &'static str,
}

/// What `arboard` genuinely delivers for an image, per platform.
///
/// Read off `arboard` 3.6's own source rather than assumed, because this is the
/// input to every claim below.
#[must_use]
pub const fn arboard_delivers(platform: ClipboardPlatform) -> &'static [&'static str] {
    match platform {
        // `Set::image` builds an `NSImage` and calls `writeObjects`. `NSImage`
        // advertises TIFF (and PDF for vector representations) as its writable
        // types; a bitmap therefore lands as TIFF alone.
        ClipboardPlatform::MacOs => &["NSPasteboardTypeTIFF"],
        // `set_image` calls both `add_png_file` and `add_cf_dibv5`.
        ClipboardPlatform::Windows => &["PNG", "CF_DIBV5"],
        ClipboardPlatform::X11 | ClipboardPlatform::Wayland => &["image/png"],
    }
}

/// The flavours D10 wants that `arboard` will not put on the clipboard.
#[must_use]
pub const fn gaps(platform: ClipboardPlatform) -> &'static [FlavourGap] {
    match platform {
        ClipboardPlatform::MacOs => &[FlavourGap {
            platform_type: "public.png",
            reason: "arboard writes an NSImage via writeObjects, which advertises only \
                     NSPasteboardTypeTIFF for a bitmap. PNG is never declared, so any \
                     application that requests public.png and does not fall back to TIFF \
                     receives nothing — and the TIFF it does get is uncompressed, so a \
                     4K capture occupies roughly 33 MB of pasteboard.",
            native_work: "Call NSPasteboard::clearContents, then setData:forType: once per \
                          flavour with the bytes from `offer`, declaring public.png before \
                          NSPasteboardTypeTIFF. That is objc2 message dispatch, so it needs \
                          an unsafe block and cannot live in this crate while it forbids \
                          unsafe code — a small platform shim crate, or a cfg-gated module \
                          with a scoped allow, is the change required.",
        }],
        ClipboardPlatform::Windows => &[FlavourGap {
            platform_type: "CF_DIB",
            reason: "arboard offers CF_DIBV5 and a registered PNG format but never CF_DIB. \
                     Windows does synthesise CF_DIB from CF_DIBV5, which covers most \
                     callers, but the synthesised bitmap keeps the alpha channel as \
                     uninterpreted bytes: an application that ignores the alpha mask shows \
                     transparent pixels as whatever colour sits underneath them, which for \
                     a premultiplied source is black.",
            native_work: "After OpenClipboard, add a third SetClipboardData for CF_DIB with \
                          the 24-bit bitmap from `offer` (already composited over white, so \
                          alpha-ignoring applications get the intended picture). This needs \
                          direct Win32 clipboard calls rather than arboard's Set::image, \
                          which clears the clipboard on entry.",
        }],
        ClipboardPlatform::X11 | ClipboardPlatform::Wayland => &[
            FlavourGap {
                platform_type: "image/bmp",
                reason: "arboard advertises only the image/png target. Several GTK2-era and \
                         Qt applications request image/bmp and treat an unavailable target \
                         as an empty clipboard.",
                native_work: "arboard's X11 backend already serves an arbitrary list of \
                              (atom, bytes) pairs internally — its ClipboardData type is \
                              exactly that — but the public API funnels images through a \
                              single PNG target. Closing this means either an upstream API \
                              taking multiple targets, or owning the selection directly \
                              with x11rb and the wl_data_device protocol.",
            },
            FlavourGap {
                platform_type: "image/tiff",
                reason: "Not offered, for the same reason as image/bmp. Qt applications \
                         commonly list it among their accepted image targets.",
                native_work: "Same as image/bmp: one more (atom, bytes) pair on the same \
                              selection owner.",
            },
        ],
    }
}

/// What actually happened, and what did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardReport {
    /// The platform written to.
    pub platform: ClipboardPlatform,
    /// The platform types the clipboard now holds.
    pub delivered: &'static [&'static str],
    /// The flavours D10 asks for that were not delivered.
    pub missing: &'static [FlavourGap],
}

impl ClipboardReport {
    /// Whether every flavour D10 asks for was delivered.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }
}

// ---------------------------------------------------------------------------
// The clipboard itself
// ---------------------------------------------------------------------------

/// A [`Clipboard`] backed by `arboard`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClipboard {
    platform: Option<ClipboardPlatform>,
}

impl SystemClipboard {
    /// A clipboard targeting the platform this build runs on.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A clipboard whose *reporting* pretends to be `platform`.
    ///
    /// The write still goes through `arboard` to the real clipboard; only the
    /// gap analysis is overridden. It exists so the report for every platform
    /// can be exercised from one machine.
    #[must_use]
    pub const fn reporting_as(platform: ClipboardPlatform) -> Self {
        Self {
            platform: Some(platform),
        }
    }

    fn platform(self) -> ClipboardPlatform {
        self.platform.unwrap_or_else(ClipboardPlatform::current)
    }

    /// Writes the capture and reports which flavours the platform received.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for a malformed frame or
    /// [`Error::Platform`] if the clipboard was unavailable — which on Linux
    /// includes there being no display server at all, the ordinary case in CI.
    pub fn write_image_reporting(&self, frame: &Frame) -> Result<ClipboardReport> {
        let image = to_straight_rgba8(frame)?;
        if frame.color_space != ColorSpace::Srgb && frame.color_space != ColorSpace::Unknown {
            // `arboard` takes raw samples with no way to attach a profile, so a
            // wide-gamut capture is pasted as though it were sRGB. Saving the
            // same capture to a file keeps its profile; the clipboard cannot.
            tracing::debug!(
                space = ?frame.color_space,
                "clipboard flavours carry no colour profile through arboard; \
                 pasted colours will be interpreted as sRGB"
            );
        }

        let mut clipboard = arboard::Clipboard::new()
            .map_err(|e| Error::Platform(format!("clipboard unavailable: {e}")))?;
        clipboard
            .set_image(arboard::ImageData {
                width: image.width as usize,
                height: image.height as usize,
                bytes: Cow::Borrowed(&image.data),
            })
            .map_err(|e| Error::Platform(format!("could not write to the clipboard: {e}")))?;

        let platform = self.platform();
        let report = ClipboardReport {
            platform,
            delivered: arboard_delivers(platform),
            missing: gaps(platform),
        };
        if !report.is_complete() {
            tracing::debug!(
                ?platform,
                delivered = ?report.delivered,
                missing = ?report.missing.iter().map(|g| g.platform_type).collect::<Vec<_>>(),
                "clipboard offer is narrower than decision D10 requires"
            );
        }
        Ok(report)
    }

    /// Writes plain text, used for a share URL after an upload succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if the system clipboard is unavailable.
    pub fn write_text(&self, text: &str) -> Result<()> {
        let mut clipboard = arboard::Clipboard::new()
            .map_err(|e| Error::Platform(format!("clipboard unavailable: {e}")))?;
        clipboard
            .set_text(text)
            .map_err(|e| Error::Platform(format!("could not write clipboard text: {e}")))
    }
}

impl Clipboard for SystemClipboard {
    fn write_image(&self, frame: &Frame) -> Result<()> {
        self.write_image_reporting(frame).map(|_| ())
    }

    fn write_image_with_report(&self, frame: &Frame) -> Result<Option<ClipboardReport>> {
        self.write_image_reporting(frame).map(Some)
    }
}

// ---------------------------------------------------------------------------
// Bitmap encodings
// ---------------------------------------------------------------------------

/// `BITMAPV5HEADER` is 124 bytes and the layout is positional; the constant is
/// written into the header and asserted against the emitted length.
const BITMAPV5HEADER_SIZE: u32 = 124;
/// `BITMAPINFOHEADER`, the 1995 vintage without masks or colour space.
const BITMAPINFOHEADER_SIZE: u32 = 40;
/// `BI_RGB`: uncompressed, channel positions implied by bit depth.
const BI_RGB: u32 = 0;
/// `BI_BITFIELDS`: uncompressed, channel positions given by explicit masks.
const BI_BITFIELDS: u32 = 3;
/// `PROFILE_EMBEDDED` — the header carries an ICC profile inline.
const PROFILE_EMBEDDED: u32 = 0x4D42_4544;
/// `LCS_sRGB`.
const LCS_SRGB: u32 = 0x7352_4742;

/// A 32-bit `BITMAPV5HEADER` DIB with alpha, as `CF_DIBV5` wants it.
///
/// Rows run bottom-up, which is the DIB default. A negative height means
/// top-down and is legal, but enough consumers mishandle it that the row
/// reversal is the safer cost.
///
/// The V5 header can carry an ICC profile inline, so unlike the raw sample
/// buffer `arboard` hands over, this flavour keeps a Display P3 capture's
/// colour.
#[must_use]
pub fn dib_v5(image: &RgbaImage, profile: Option<&[u8]>) -> Vec<u8> {
    let pixels = bgra_bottom_up(image);
    let profile = profile.unwrap_or(&[]);
    let mut out = Vec::with_capacity(BITMAPV5HEADER_SIZE as usize + pixels.len() + profile.len());

    out.extend_from_slice(&BITMAPV5HEADER_SIZE.to_le_bytes());
    out.extend_from_slice(&(image.width as i32).to_le_bytes());
    out.extend_from_slice(&(image.height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&BI_BITFIELDS.to_le_bytes());
    out.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes()); // x pixels per metre
    out.extend_from_slice(&0i32.to_le_bytes()); // y pixels per metre
    out.extend_from_slice(&0u32.to_le_bytes()); // colours used
    out.extend_from_slice(&0u32.to_le_bytes()); // colours important
    out.extend_from_slice(&0x00FF_0000u32.to_le_bytes()); // red mask
    out.extend_from_slice(&0x0000_FF00u32.to_le_bytes()); // green mask
    out.extend_from_slice(&0x0000_00FFu32.to_le_bytes()); // blue mask
    out.extend_from_slice(&0xFF00_0000u32.to_le_bytes()); // alpha mask
    out.extend_from_slice(
        &if profile.is_empty() {
            LCS_SRGB
        } else {
            PROFILE_EMBEDDED
        }
        .to_le_bytes(),
    );
    out.extend_from_slice(&[0u8; 36]); // CIEXYZTRIPLE endpoints, unused
    out.extend_from_slice(&[0u8; 12]); // per-channel gamma, unused
    out.extend_from_slice(&0u32.to_le_bytes()); // rendering intent
    // Profile offset is measured from the start of the header, and the profile
    // is placed after the pixels.
    out.extend_from_slice(
        &if profile.is_empty() {
            0
        } else {
            BITMAPV5HEADER_SIZE + pixels.len() as u32
        }
        .to_le_bytes(),
    );
    out.extend_from_slice(&(profile.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved
    debug_assert_eq!(out.len(), BITMAPV5HEADER_SIZE as usize);

    out.extend_from_slice(&pixels);
    out.extend_from_slice(profile);
    out
}

/// A 24-bit `BITMAPINFOHEADER` DIB, as `CF_DIB` wants it.
///
/// Alpha is composited away rather than dropped. That is the entire reason this
/// flavour is worth offering alongside the V5 one: applications that take
/// `CF_DIB` are exactly the applications that ignore an alpha mask, and handing
/// them 32-bit data means a transparent capture pastes as black rectangles.
#[must_use]
pub fn dib_24(image: &RgbaImage, background: [u8; 3]) -> Vec<u8> {
    let rgb = image.to_rgb8(background);
    // Every DIB row is padded to a four-byte boundary. Forgetting this is the
    // other classic diagonal-skew bug.
    let row = image.width as usize * 3;
    let padded = row.next_multiple_of(4);
    let mut pixels = Vec::with_capacity(padded * image.height as usize);
    for y in (0..image.height as usize).rev() {
        for px in rgb[y * row..(y + 1) * row].as_chunks::<3>().0 {
            pixels.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        pixels.resize(pixels.len().next_multiple_of(4), 0);
    }

    let mut out = Vec::with_capacity(BITMAPINFOHEADER_SIZE as usize + pixels.len());
    out.extend_from_slice(&BITMAPINFOHEADER_SIZE.to_le_bytes());
    out.extend_from_slice(&(image.width as i32).to_le_bytes());
    out.extend_from_slice(&(image.height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&BI_RGB.to_le_bytes());
    out.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    debug_assert_eq!(out.len(), BITMAPINFOHEADER_SIZE as usize);
    out.extend_from_slice(&pixels);
    out
}

/// A `.bmp` file: a `BITMAPFILEHEADER` in front of a V5 DIB.
#[must_use]
pub fn bmp(image: &RgbaImage, profile: Option<&[u8]>) -> Vec<u8> {
    let dib = dib_v5(image, profile);
    let mut out = Vec::with_capacity(14 + dib.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&(14 + BITMAPV5HEADER_SIZE).to_le_bytes()); // offset to pixels
    out.extend_from_slice(&dib);
    out
}

/// Blue-green-red-alpha samples, last row first.
fn bgra_bottom_up(image: &RgbaImage) -> Vec<u8> {
    let row = image.width as usize * 4;
    let mut out = Vec::with_capacity(row * image.height as usize);
    for y in (0..image.height as usize).rev() {
        for px in image.data[y * row..(y + 1) * row].as_chunks::<4>().0 {
            out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    out
}

/// A baseline uncompressed RGBA TIFF.
///
/// Written by hand because `image` is built here without its TIFF feature, and
/// enabling it would mean editing a shared manifest to gain an encoder far more
/// general than one uncompressed strip. The result is what macOS calls
/// `NSPasteboardTypeTIFF`.
///
/// Alpha is tagged `ExtraSamples = 2`, unassociated — which is TIFF's name for
/// straight alpha. Tagging premultiplied data as unassociated is the same black
/// fringe as everywhere else in this crate.
#[must_use]
pub fn tiff(image: &RgbaImage, profile: Option<&[u8]>) -> Vec<u8> {
    const SHORT: u16 = 3;
    const LONG: u16 = 4;
    const RATIONAL: u16 = 5;
    const UNDEFINED: u16 = 7;

    let profile = profile.unwrap_or(&[]);
    // Values wider than four bytes live outside the directory and are pointed
    // at, so their offsets must be known before the directory is written.
    let entries = 13 + u16::from(!profile.is_empty());
    let directory_end = 8 + 2 + u32::from(entries) * 12 + 4;
    let bits_offset = directory_end;
    let x_resolution_offset = bits_offset + 8;
    let y_resolution_offset = x_resolution_offset + 8;
    let profile_offset = y_resolution_offset + 8;
    // TIFF offsets must be even; a profile of odd length would misalign the
    // pixel strip that follows it.
    let pixels_offset = (profile_offset + profile.len() as u32).next_multiple_of(2);
    let pixel_bytes = image.width * image.height * 4;

    let mut directory = Vec::with_capacity(usize::from(entries) * 12 + 6);
    directory.extend_from_slice(&entries.to_le_bytes());
    let mut entry = |tag: u16, kind: u16, count: u32, value: u32| {
        directory.extend_from_slice(&tag.to_le_bytes());
        directory.extend_from_slice(&kind.to_le_bytes());
        directory.extend_from_slice(&count.to_le_bytes());
        // A value narrow enough to fit is stored in place, left-aligned, which
        // for little-endian means simply writing it as a u32.
        directory.extend_from_slice(&value.to_le_bytes());
    };

    // Tags must appear in ascending order; readers are entitled to binary-search.
    entry(256, LONG, 1, image.width); // ImageWidth
    entry(257, LONG, 1, image.height); // ImageLength
    entry(258, SHORT, 4, bits_offset); // BitsPerSample
    entry(259, SHORT, 1, 1); // Compression: none
    entry(262, SHORT, 1, 2); // PhotometricInterpretation: RGB
    entry(273, LONG, 1, pixels_offset); // StripOffsets
    entry(277, SHORT, 1, 4); // SamplesPerPixel
    entry(278, LONG, 1, image.height); // RowsPerStrip: one strip
    entry(279, LONG, 1, pixel_bytes); // StripByteCounts
    entry(282, RATIONAL, 1, x_resolution_offset); // XResolution
    entry(283, RATIONAL, 1, y_resolution_offset); // YResolution
    entry(296, SHORT, 1, 2); // ResolutionUnit: inch
    entry(338, SHORT, 1, 2); // ExtraSamples: unassociated alpha
    if !profile.is_empty() {
        entry(34_675, UNDEFINED, profile.len() as u32, profile_offset);
    }
    directory.extend_from_slice(&0u32.to_le_bytes()); // no further directories

    let mut out = Vec::with_capacity(pixels_offset as usize + pixel_bytes as usize);
    out.extend_from_slice(b"II"); // little-endian
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes()); // directory follows the header
    out.extend_from_slice(&directory);
    debug_assert_eq!(out.len() as u32, directory_end);

    out.extend_from_slice(&[8u16, 8, 8, 8].map(u16::to_le_bytes).concat());
    for _ in 0..2 {
        out.extend_from_slice(&72u32.to_le_bytes()); // numerator
        out.extend_from_slice(&1u32.to_le_bytes()); // denominator
    }
    out.extend_from_slice(profile);
    out.resize(pixels_offset as usize, 0);
    out.extend_from_slice(&image.data);
    out
}
