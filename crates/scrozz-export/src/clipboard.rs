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
//! Image data and file references are committed in one clipboard replacement.
//! This is what makes the `image-and-file` setting real rather than two writes
//! where the second silently erases the first. Every backend is verified after
//! the write; a backend that reports success while omitting a required flavour
//! is an error, not an optimistic success report.

use std::path::PathBuf;

use scrozz_core::{Error, Frame, Result};

use crate::{
    Clipboard, ImageFormat,
    encode::{EncodeOptions, FrameEncoder, PngEffort},
    icc::profile_for,
    pixels::{RgbaImage, to_straight_rgba8},
};

/// The clipboard implementation in play, which is not simply the OS.
///
/// X11 and Wayland are separate because they are separate protocols with
/// separate ownership models, and the clipboard library implements them
/// separately.
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
    /// Linux chooses Wayland only when this process is connected to a Wayland
    /// display; otherwise the X11 selection backend owns the clipboard.
    #[must_use]
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            Self::Wayland
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
            (Self::Tiff, ClipboardPlatform::MacOs) => "public.tiff",
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

/// A flavour D10 calls for that a clipboard backend does not deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlavourGap {
    /// The platform type that is missing.
    pub platform_type: &'static str,
    /// Why the active backend does not offer it.
    pub reason: &'static str,
    /// The native work that would close the gap.
    pub native_work: &'static str,
}

/// Image flavours Scrozz commits and verifies on each platform.
#[must_use]
pub const fn backend_delivers(platform: ClipboardPlatform) -> &'static [&'static str] {
    match platform {
        ClipboardPlatform::MacOs => &["public.png", "public.tiff"],
        ClipboardPlatform::Windows => &["PNG", "CF_DIBV5", "CF_DIB"],
        ClipboardPlatform::X11 | ClipboardPlatform::Wayland => {
            &["image/png", "image/bmp", "image/tiff"]
        }
    }
}

/// Required image flavours the backend still cannot deliver.
///
/// Empty because every flavour in [`preferred_kinds`] is now committed and
/// verified before a copy is reported as successful.
#[must_use]
pub const fn gaps(_platform: ClipboardPlatform) -> &'static [FlavourGap] {
    &[]
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

/// What one verified clipboard replacement contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardDelivery {
    /// Whether image representations were committed.
    pub image: bool,
    /// Number of file references committed.
    pub files: usize,
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

/// The native system clipboard.
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

    /// A clipboard whose image report pretends to be `platform`.
    ///
    /// The write still targets the real host clipboard; only the report is
    /// overridden. It exists so report formatting can be exercised anywhere.
    #[must_use]
    pub const fn reporting_as(platform: ClipboardPlatform) -> Self {
        Self {
            platform: Some(platform),
        }
    }

    fn platform(self) -> ClipboardPlatform {
        self.platform.unwrap_or_else(ClipboardPlatform::current)
    }

    /// Replaces the clipboard with image data, file references, or both.
    ///
    /// Image and file representations are committed in one operation. Every
    /// requested representation is then queried from the native clipboard; a
    /// partial write is returned as an error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when both inputs are empty, a file is
    /// relative, missing, or not Unicode, or the frame is malformed. Returns
    /// [`Error::Platform`] when the clipboard cannot be opened or verified.
    pub fn write_content(
        &self,
        image: Option<&Frame>,
        files: &[PathBuf],
    ) -> Result<ClipboardDelivery> {
        if image.is_none() && files.is_empty() {
            return Err(Error::InvalidRequest(
                "clipboard content must include an image, a file, or both".to_owned(),
            ));
        }
        validate_files(files)?;

        let platform = ClipboardPlatform::current();
        let flavours = image.map(|frame| offer(frame, platform)).transpose()?;
        write_platform(platform, flavours.as_deref().unwrap_or_default(), files)?;
        Ok(ClipboardDelivery {
            image: image.is_some(),
            files: files.len(),
        })
    }

    /// Writes the capture and reports which flavours the platform received.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for a malformed frame or
    /// [`Error::Platform`] if the clipboard was unavailable — which on Linux
    /// includes there being no display server at all, the ordinary case in CI.
    pub fn write_image_reporting(&self, frame: &Frame) -> Result<ClipboardReport> {
        self.write_content(Some(frame), &[])?;

        let platform = self.platform();
        let report = ClipboardReport {
            platform,
            delivered: backend_delivers(platform),
            missing: gaps(platform),
        };
        Ok(report)
    }
}

fn validate_files(files: &[PathBuf]) -> Result<()> {
    for path in files {
        if !path.is_absolute() {
            return Err(Error::InvalidRequest(format!(
                "clipboard file references must be absolute: {}",
                path.display()
            )));
        }
        if !path.is_file() {
            return Err(Error::InvalidRequest(format!(
                "clipboard file reference does not exist: {}",
                path.display()
            )));
        }
        if path.to_str().is_none() {
            return Err(Error::InvalidRequest(format!(
                "clipboard file reference is not valid Unicode: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn write_platform(
    platform: ClipboardPlatform,
    flavours: &[Flavour],
    files: &[PathBuf],
) -> Result<()> {
    use clipboard_rs::{Clipboard as _, ClipboardContent, ClipboardContext};

    #[cfg(target_os = "macos")]
    let clipboard = ClipboardContext::new()
        .map_err(|error| Error::Platform(format!("clipboard unavailable: {error}")))?;
    #[cfg(not(target_os = "macos"))]
    let clipboard_guard = {
        use std::sync::{Mutex, OnceLock};

        static CLIPBOARD: OnceLock<Mutex<Option<ClipboardContext>>> = OnceLock::new();
        let mut guard = CLIPBOARD
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| Error::Platform("clipboard context lock was poisoned".to_owned()))?;
        if guard.is_none() {
            *guard = Some(
                ClipboardContext::new()
                    .map_err(|error| Error::Platform(format!("clipboard unavailable: {error}")))?,
            );
        }
        guard
    };
    #[cfg(not(target_os = "macos"))]
    let clipboard = clipboard_guard
        .as_ref()
        .expect("clipboard context was initialized");
    let mut contents = flavours
        .iter()
        .map(|flavour| {
            ClipboardContent::Other(flavour.platform_type.to_owned(), flavour.bytes.clone())
        })
        .collect::<Vec<_>>();
    if !files.is_empty() {
        contents.push(ClipboardContent::Files(
            files
                .iter()
                .map(|path| path.to_str().expect("validated as Unicode").to_owned())
                .collect(),
        ));
    }
    clipboard
        .set(contents)
        .map_err(|error| Error::Platform(format!("could not write to the clipboard: {error}")))?;

    let expected_file_type = file_platform_type(platform);
    let mut missing = Vec::new();
    for _ in 0..5 {
        let available = clipboard.available_formats().map_err(|error| {
            Error::Platform(format!("could not verify clipboard formats: {error}"))
        })?;
        missing = flavours
            .iter()
            .map(|flavour| flavour.platform_type)
            .chain((!files.is_empty()).then_some(expected_file_type))
            .filter(|required| !available.iter().any(|actual| actual == required))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err(Error::Platform(format!(
        "clipboard omitted required format(s): {}",
        missing.join(", ")
    )))
}

#[cfg(target_os = "windows")]
fn write_platform(
    _platform: ClipboardPlatform,
    flavours: &[Flavour],
    files: &[PathBuf],
) -> Result<()> {
    use clipboard_win::{
        Clipboard as WindowsClipboard, formats, options,
        raw::{set_file_list_with, set_without_clear},
    };

    let _clipboard = WindowsClipboard::new_attempts(10)
        .map_err(|code| Error::Platform(format!("clipboard unavailable: error {code}")))?;
    clipboard_win::empty()
        .map_err(|code| Error::Platform(format!("could not clear the clipboard: error {code}")))?;

    for flavour in flavours {
        let format = match flavour.kind {
            FlavourKind::Png => clipboard_win::register_format("PNG")
                .ok_or_else(|| Error::Platform("could not register the PNG format".to_owned()))?
                .get(),
            FlavourKind::DibV5 => formats::CF_DIBV5,
            FlavourKind::Dib => formats::CF_DIB,
            FlavourKind::Tiff | FlavourKind::Bmp => {
                return Err(Error::InvalidRequest(format!(
                    "unexpected Windows clipboard flavour: {:?}",
                    flavour.kind
                )));
            }
        };
        set_without_clear(format, &flavour.bytes).map_err(|code| {
            Error::Platform(format!(
                "could not write {} to the clipboard: error {code}",
                flavour.platform_type
            ))
        })?;
    }

    if !files.is_empty() {
        let paths = files
            .iter()
            .map(|path| path.to_str().expect("validated as Unicode").to_owned())
            .collect::<Vec<_>>();
        set_file_list_with(&paths, options::NoClear).map_err(|code| {
            Error::Platform(format!(
                "could not write file references to the clipboard: error {code}"
            ))
        })?;
    }

    let png = clipboard_win::register_format("PNG")
        .ok_or_else(|| Error::Platform("could not verify the PNG format".to_owned()))?
        .get();
    let missing = flavours
        .iter()
        .filter_map(|flavour| {
            let format = match flavour.kind {
                FlavourKind::Png => png,
                FlavourKind::DibV5 => formats::CF_DIBV5,
                FlavourKind::Dib => formats::CF_DIB,
                FlavourKind::Tiff | FlavourKind::Bmp => return Some(flavour.platform_type),
            };
            (!clipboard_win::is_format_avail(format)).then_some(flavour.platform_type)
        })
        .chain(
            (!files.is_empty() && !clipboard_win::is_format_avail(formats::CF_HDROP))
                .then_some(file_platform_type(ClipboardPlatform::Windows)),
        )
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Error::Platform(format!(
            "clipboard omitted required format(s): {}",
            missing.join(", ")
        )))
    }
}

const fn file_platform_type(platform: ClipboardPlatform) -> &'static str {
    match platform {
        ClipboardPlatform::MacOs => "public.file-url",
        ClipboardPlatform::Windows => "CF_HDROP",
        ClipboardPlatform::X11 | ClipboardPlatform::Wayland => "text/uri-list",
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
