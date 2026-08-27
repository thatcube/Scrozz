//! Dual Windows OCR for packaged and portable Scrozz artifacts.
//!
//! A process with package identity uses `Windows.Media.Ocr`. An unpackaged
//! process uses the artifact-local Tesseract payload. Selection happens before
//! recognition, so a real native OCR error is never hidden by a retry.
//!
//! # Why the OS engine
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

use crate::apartment;
use scrozz_core::{Error, Frame, Result};
use scrozz_shell::windows::identity::PackageIdentity;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::core::HSTRING;

use crate::layout;
use crate::prepare::{self, Prepared};
use crate::windows_runtime::{
    Backend, backend_for_package_identity, bgra_premultiplied_on_white, dispatch,
};
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

fn active_backend() -> Result<Backend> {
    match scrozz_shell::windows::identity::current() {
        PackageIdentity::Packaged { .. } => Ok(backend_for_package_identity(true)),
        PackageIdentity::Unpackaged => Ok(backend_for_package_identity(false)),
        PackageIdentity::Unknown { status, detail } => Err(Error::Platform(format!(
            "cannot select the Windows OCR backend because package identity is unknown \
             (Win32 status {status}): {detail}"
        ))),
    }
}

/// Name of the backend selected for this process artifact.
pub(crate) fn engine_name() -> Result<&'static str> {
    Ok(active_backend()?.engine_name())
}

/// Recognises text using the one backend selected from process package identity.
///
/// # Errors
///
/// [`Error::InvalidRequest`] for a malformed frame, [`Error::Unsupported`] if
/// the selected backend or requested language is unavailable, and
/// [`Error::Platform`] for native or subprocess failures.
pub fn recognize(frame: &Frame, options: &Options) -> Result<Vec<TextBlock>> {
    dispatch(
        active_backend()?,
        || recognize_native(frame, options),
        || crate::tesseract::recognize(frame, options),
    )
}

fn recognize_native(frame: &Frame, options: &Options) -> Result<Vec<TextBlock>> {
    let apartment = apartment::Apartment::enter_multithreaded()?;
    tracing::debug!(
        owned = apartment.owns(),
        "Windows OCR caller has a COM apartment"
    );

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
        // A missing engine is usually a configuration gap the user can close,
        // so it gets an Unsupported with the remedy rather than an opaque
        // HRESULT. Usually — but not when the thread never entered a COM
        // apartment, in which case that remedy is advice to install a language
        // pack the user already has, for a fault that is entirely ours.
        return OcrEngine::TryCreateFromUserProfileLanguages()
            .map_err(|e| apartment::engine_failure(e.code().0, "text recognition", &e.message()));
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

    // Every candidate was rejected by `IsLanguageSupported`, which answers
    // `Err(CO_E_NOTINITIALIZED)` on an uninitialised thread and is read here as
    // a plain "no". Ask once, directly, so that case is named rather than
    // reported as a machine-wide absence of language packs.
    if let Err(e) = OcrEngine::AvailableRecognizerLanguages()
        && apartment::is_uninitialised_apartment(e.code().0)
    {
        return Err(apartment::engine_failure(
            e.code().0,
            "text recognition",
            &e.message(),
        ));
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
    let Ok(available) = OcrEngine::AvailableRecognizerLanguages() else {
        return "unknown".to_string();
    };
    let tags: Vec<String> = (0..available.Size().unwrap_or(0))
        .filter_map(|i| available.GetAt(i).ok())
        .filter_map(|language| language.LanguageTag().ok())
        .map(|tag| tag.to_string_lossy())
        .collect();
    if tags.is_empty() {
        "none".to_string()
    } else {
        tags.join(", ")
    }
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
        .WriteBytes(&bgra_premultiplied_on_white(&prepared.image))
        .map_err(|e| Error::Platform(format!("DataWriter::WriteBytes failed: {e}")))?;
    let buffer = writer
        .DetachBuffer()
        .map_err(|e| Error::Platform(format!("DataWriter::DetachBuffer failed: {e}")))?;

    // `CreateCopyFromBuffer` assumes rows are tightly packed at `width * 4`,
    // which is exactly what `Rgba8Image` guarantees, so there is no stride to
    // reconcile. Transparent pixels were composited on white and the resulting
    // opaque bytes are valid premultiplied input.
    SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, width, height)
        .map_err(|e| Error::Platform(format!("SoftwareBitmap::CreateCopyFromBuffer failed: {e}")))
}
