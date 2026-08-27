//! Dual Windows OCR for packaged and portable Scrozz artifacts.
//!
//! A process with package identity uses `Windows.Media.Ocr`. The portable ZIP
//! intentionally remains an unpackaged executable, where that API cannot be
//! activated reliably, so it uses the same local Tesseract subprocess backend as
//! Linux. Backend selection happens once from package identity; a real native OCR
//! failure is never hidden by silently retrying with Tesseract.
//!
//! # Why the native engine
//!
//! Same reasoning as macOS: it ships with Windows, it is already localised to
//! the languages the user installed, it costs nothing in binary size, and it is
//! tuned for screen content. `OcrEngine::TryCreateFromUserProfileLanguages` is
//! exactly the "use the user's locale" behaviour we want, with no configuration.
//!
//! # Language selection
//!
//! [`Options::languages`](crate::Options::languages) is authoritative here. Each
//! requested tag is tried in priority order against
//! `OcrEngine::TryCreateFromLanguage`, and the first one with an installed
//! recognizer wins. An empty list means "use the user's own display languages",
//! which is `TryCreateFromUserProfileLanguages`.
//!
//! When *none* of the requested languages has a pack this fails rather than
//! quietly falling back to the profile engine. Recognising German text with an
//! English recogniser does not produce an error, it produces plausible-looking
//! nonsense, and a caller that asked for a specific language has no way to tell
//! that apart from a genuinely bad scan. The error names both what was asked for
//! and what is actually installed, so the caller can retry with something real.
//!
//! # Coordinates
//!
//! Windows reports top-left pixel rectangles — no vertical flip — but they are in
//! the coordinate space of the bitmap *handed to the engine*, which is the
//! upscaled one. [`crate::layout::pixels_to_physical`] divides that back out.
//! `OcrLine` carries no rectangle of its own, so a line's bounds are the union
//! of its words'.

use std::time::{Duration, Instant};

use scrozz_core::{Error, Frame, Result};
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::{
    Foundation::{
        APPMODEL_ERROR_NO_PACKAGE, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, WIN32_ERROR,
    },
    Storage::Packaging::Appx::GetCurrentPackageFullName,
};
use windows::core::{HSTRING, PWSTR};

use crate::layout;
use crate::prepare::{self, Prepared, Rgba8Image};
use crate::{Options, TextBlock};

/// `AsyncStatus::Started`. Spelled out because `windows_future` is not a direct
/// dependency and its enum cannot be named here; the values are fixed WinRT ABI.
const ASYNC_STARTED: i32 = 0;
/// `AsyncStatus::Completed`.
const ASYNC_COMPLETED: i32 = 1;
/// `AsyncStatus::Canceled`.
const ASYNC_CANCELED: i32 = 2;

/// How long to wait for one recognition before giving up.
///
/// Generous — recognition of a screenshot is tens of milliseconds — but finite,
/// so a wedged engine surfaces as an error instead of a hung UI thread.
const RECOGNITION_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    WindowsMediaOcr,
    Tesseract,
}

const fn backend_for_package_identity(has_identity: bool) -> Backend {
    if has_identity {
        Backend::WindowsMediaOcr
    } else {
        Backend::Tesseract
    }
}

fn active_backend() -> Result<Backend> {
    let backend = backend_for_package_identity(has_package_identity()?);
    if backend == Backend::Tesseract && !cfg!(feature = "tesseract") {
        return Err(Error::Unsupported {
            what: "text recognition from the portable Windows artifact".to_string(),
            why: "this unpackaged process cannot use Windows.Media.Ocr and this build disabled \
                  the unpackaged-safe `tesseract` feature"
                .to_string(),
        });
    }
    Ok(backend)
}

/// Name of the backend selected for this process artifact.
pub fn engine_name() -> Result<&'static str> {
    Ok(match active_backend()? {
        Backend::WindowsMediaOcr => "windows-media-ocr",
        Backend::Tesseract => "tesseract",
    })
}

fn has_package_identity() -> Result<bool> {
    let mut length = 0u32;
    let status = unsafe { GetCurrentPackageFullName(&raw mut length, None) };
    if status == APPMODEL_ERROR_NO_PACKAGE {
        return Ok(false);
    }
    if status == ERROR_SUCCESS {
        return Ok(true);
    }
    if status != ERROR_INSUFFICIENT_BUFFER {
        return Err(package_identity_error(status));
    }

    let mut name = vec![0u16; length as usize];
    let status =
        unsafe { GetCurrentPackageFullName(&raw mut length, Some(PWSTR(name.as_mut_ptr()))) };
    if status == ERROR_SUCCESS {
        Ok(true)
    } else if status == APPMODEL_ERROR_NO_PACKAGE {
        Ok(false)
    } else {
        Err(package_identity_error(status))
    }
}

fn package_identity_error(status: WIN32_ERROR) -> Error {
    Error::Platform(format!(
        "GetCurrentPackageFullName failed with Win32 error {}",
        status.0
    ))
}

/// Recognises text in a frame using the Windows OCR engine.
///
/// # Errors
///
/// [`Error::InvalidRequest`] for a malformed frame, [`Error::Unsupported`] if
/// the machine has no OCR language pack installed, [`Error::Platform`] for any
/// other WinRT failure.
pub fn recognize(frame: &Frame, options: &Options) -> Result<Vec<TextBlock>> {
    match active_backend()? {
        Backend::WindowsMediaOcr => recognize_native(frame, options),
        Backend::Tesseract => {
            #[cfg(feature = "tesseract")]
            {
                crate::tesseract::recognize(frame, options)
            }
            #[cfg(not(feature = "tesseract"))]
            {
                let _ = (frame, options);
                unreachable!("active_backend rejects this configuration")
            }
        }
    }
}

fn recognize_native(frame: &Frame, options: &Options) -> Result<Vec<TextBlock>> {
    if options.automatic_language_detection {
        return Err(Error::Unsupported {
            what: "automatic OCR language detection".to_string(),
            why: "Windows.Media.Ocr requires an installed language pack selected explicitly or \
                  through the user's profile; it cannot infer language from image content"
                .to_string(),
        });
    }

    // Ask the engine for its ceiling first: the answer feeds the upscale
    // decision, so an image is never enlarged past what the engine will accept
    // and an already-oversized capture is shrunk rather than rejected.
    let max_dimension = OcrEngine::MaxImageDimension().ok();
    let prepared = prepare::prepare(frame, options.upscale, max_dimension)?;

    let engine = engine_for(&options.languages)?;
    if let Ok(tag) = engine
        .RecognizerLanguage()
        .and_then(|language| language.LanguageTag())
    {
        tracing::debug!(language = %tag, "Windows OCR recogniser selected");
    }

    let bitmap = software_bitmap(&prepared)?;
    let operation = engine
        .RecognizeAsync(&bitmap)
        .map_err(|e| Error::Platform(format!("OcrEngine::RecognizeAsync failed: {e}")))?;

    // `windows_future::Async::join` would be the natural blocking wait, but that
    // trait lives in a crate this one does not depend on directly and so cannot
    // be imported. Polling the inherent `Status` is the portable alternative.
    let deadline = Instant::now() + RECOGNITION_TIMEOUT;
    let mut backoff = Duration::from_micros(200);
    loop {
        let status = operation
            .Status()
            .map_err(|e| Error::Platform(format!("IAsyncOperation::Status failed: {e}")))?;
        match status.0 {
            ASYNC_COMPLETED => break,
            ASYNC_CANCELED => return Err(Error::Cancelled),
            ASYNC_STARTED => {}
            // Error: `GetResults` carries the actual HRESULT, so fall through
            // and let it produce the message.
            _ => break,
        }
        if Instant::now() >= deadline {
            let _ = operation.Cancel();
            return Err(Error::Platform(format!(
                "Windows OCR did not finish within {RECOGNITION_TIMEOUT:?}"
            )));
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(4));
    }

    let result = operation
        .GetResults()
        .map_err(|e| Error::Platform(format!("Windows OCR failed: {e}")))?;
    let lines = result
        .Lines()
        .map_err(|e| Error::Platform(format!("OcrResult::Lines failed: {e}")))?;

    let source = prepared.source_size;
    let upscale = prepared.upscale;
    let mut blocks = Vec::new();

    for index in 0..lines.Size().unwrap_or(0) {
        let Ok(line) = lines.GetAt(index) else {
            continue;
        };
        let text = line.Text().map(|t| t.to_string_lossy()).unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }

        // OcrLine has no bounding rectangle, so build one from its words.
        let mut bounds = scrozz_core::PhysicalRect::default();
        if let Ok(words) = line.Words() {
            for w in 0..words.Size().unwrap_or(0) {
                let Ok(word) = words.GetAt(w) else {
                    continue;
                };
                let Ok(rect) = word.BoundingRect() else {
                    continue;
                };
                bounds = layout::union(
                    bounds,
                    layout::pixels_to_physical(
                        f64::from(rect.X),
                        f64::from(rect.Y),
                        f64::from(rect.Width),
                        f64::from(rect.Height),
                        upscale,
                        source,
                    ),
                );
            }
        }
        if bounds.is_empty() {
            continue;
        }

        blocks.push(TextBlock {
            text,
            bounds: layout::to_logical(bounds, frame.scale),
            // Windows.Media.Ocr exposes no confidence value at any level — not
            // on OcrResult, OcrLine or OcrWord. Reporting a fabricated spread
            // would be worse than reporting none, so this follows the
            // convention Apple states for its own observations: return 1.0 when
            // confidence has no meaning. Callers that need to discriminate
            // should treat a uniform 1.0 as "unknown".
            confidence: 1.0,
        });
    }

    Ok(layout::sort_reading_order(blocks))
}

/// Builds an engine for the requested languages, or for the user's own.
///
/// Requested tags are tried in priority order; the first with an installed
/// recognizer wins.
///
/// # Errors
///
/// [`Error::Unsupported`] when no requested language has a pack, or when the
/// machine has no OCR packs at all.
fn engine_for(languages: &[String]) -> Result<OcrEngine> {
    if languages.is_empty() {
        // A missing engine is a configuration gap the user can close, not a bug,
        // so it gets an Unsupported with the remedy rather than an opaque HRESULT.
        return OcrEngine::TryCreateFromUserProfileLanguages().map_err(|e| Error::Unsupported {
            what: "text recognition".to_string(),
            why: format!(
                "Windows has no OCR language pack for your display languages. \
                 Add one in Settings > Time & language > Language & region > \
                 Add a language, choosing a language whose optional features \
                 include Optical character recognition ({e})"
            ),
        });
    }

    for tag in languages {
        let hstring = HSTRING::from(tag.as_str());
        // IsWellFormed first: CreateLanguage throws on a malformed tag, and a
        // caller passing "english" instead of "en" should skip to the next
        // candidate rather than take down the whole request.
        if !Language::IsWellFormed(&hstring).unwrap_or(false) {
            tracing::debug!(tag = %tag, "not a well-formed BCP-47 language tag, skipping");
            continue;
        }
        let Ok(language) = Language::CreateLanguage(&hstring) else {
            continue;
        };
        if !OcrEngine::IsLanguageSupported(&language).unwrap_or(false) {
            continue;
        }
        if let Ok(engine) = OcrEngine::TryCreateFromLanguage(&language) {
            return Ok(engine);
        }
    }

    Err(Error::Unsupported {
        what: format!("text recognition in {}", languages.join(", ")),
        why: format!(
            "Windows has no OCR language pack for any of the requested languages. \
             Installed recognizers: {}. Add another in Settings > Time & language > \
             Language & region > Add a language, choosing one whose optional \
             features include Optical character recognition",
            installed_languages()
        ),
    })
}

/// The recognizer languages this machine has installed, for error messages.
fn installed_languages() -> String {
    let Ok(tags) = available_native_languages() else {
        return "unknown".to_string();
    };
    if tags.is_empty() {
        "none".to_string()
    } else {
        tags.join(", ")
    }
}

/// Lists OCR recognizer languages installed in Windows.
pub fn available_languages() -> Result<Vec<String>> {
    match active_backend()? {
        Backend::WindowsMediaOcr => available_native_languages(),
        Backend::Tesseract => {
            #[cfg(feature = "tesseract")]
            {
                crate::tesseract::available_languages()
            }
            #[cfg(not(feature = "tesseract"))]
            {
                unreachable!("active_backend rejects this configuration")
            }
        }
    }
}

fn available_native_languages() -> Result<Vec<String>> {
    let available = OcrEngine::AvailableRecognizerLanguages().map_err(|error| {
        Error::Platform(format!(
            "OcrEngine::AvailableRecognizerLanguages failed: {error}"
        ))
    })?;
    let mut tags: Vec<String> = (0..available.Size().unwrap_or(0))
        .filter_map(|index| available.GetAt(index).ok())
        .filter_map(|language| language.LanguageTag().ok())
        .map(|tag| tag.to_string_lossy())
        .collect();
    tags.sort_unstable();
    tags.dedup();
    Ok(tags)
}

/// Copies a prepared image into a `SoftwareBitmap` the engine can consume.
fn software_bitmap(prepared: &Prepared) -> Result<SoftwareBitmap> {
    let width = i32::try_from(prepared.image.width)
        .map_err(|_| Error::InvalidRequest("image is too wide for Windows OCR".to_string()))?;
    let height = i32::try_from(prepared.image.height)
        .map_err(|_| Error::InvalidRequest("image is too tall for Windows OCR".to_string()))?;

    // `DataWriter` is the supported way to turn a byte slice into an `IBuffer`
    // without reaching for `IBufferByteAccess`.
    let writer =
        DataWriter::new().map_err(|e| Error::Platform(format!("DataWriter::new failed: {e}")))?;
    writer
        .WriteBytes(&bgra_on_white(&prepared.image))
        .map_err(|e| Error::Platform(format!("DataWriter::WriteBytes failed: {e}")))?;
    let buffer = writer
        .DetachBuffer()
        .map_err(|e| Error::Platform(format!("DataWriter::DetachBuffer failed: {e}")))?;

    // `CreateCopyFromBuffer` assumes rows are tightly packed at `width * 4`,
    // which is exactly what `Rgba8Image` guarantees, so there is no stride to
    // reconcile. This overload does not declare an alpha mode, so the upload is
    // deliberately opaque after compositing transparency onto white.
    SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, width, height)
        .map_err(|e| Error::Platform(format!("SoftwareBitmap::CreateCopyFromBuffer failed: {e}")))
}

/// Converts straight-alpha RGBA into opaque BGRA composited on white.
fn bgra_on_white(image: &Rgba8Image) -> Vec<u8> {
    let mut out = vec![0u8; image.data.len()];
    for (s, d) in image
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        let a = s[3];
        d[0] = composite_on_white(s[2], a);
        d[1] = composite_on_white(s[1], a);
        d[2] = composite_on_white(s[0], a);
        d[3] = 255;
    }
    out
}

fn composite_on_white(channel: u8, alpha: u8) -> u8 {
    let foreground = u32::from(channel) * u32::from(alpha);
    let background = 255 * u32::from(255 - alpha);
    ((foreground + background + 127) / 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_artifacts_choose_the_unpackaged_safe_backend() {
        assert_eq!(backend_for_package_identity(false), Backend::Tesseract);
    }

    #[test]
    fn packaged_artifacts_choose_the_native_backend() {
        assert_eq!(backend_for_package_identity(true), Backend::WindowsMediaOcr);
    }

    #[test]
    fn bitmap_upload_composites_transparency_onto_white() {
        let image = Rgba8Image::new(
            3,
            1,
            vec![
                0, 0, 0, 0, // transparent black
                0, 0, 0, 128, // half-transparent black
                1, 2, 3, 255, // opaque RGB
            ],
        )
        .expect("fixture");

        assert_eq!(
            bgra_on_white(&image),
            [
                255, 255, 255, 255, //
                127, 127, 127, 255, //
                3, 2, 1, 255,
            ]
        );
    }
}
