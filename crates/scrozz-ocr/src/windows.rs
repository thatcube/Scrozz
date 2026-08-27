//! Windows text recognition via `Windows.Media.Ocr`.
//!
//! # Why the OS engine
//!
//! Same reasoning as macOS: it ships with Windows, it is already localised to
//! the languages the user installed, it costs nothing in binary size, and it is
//! tuned for screen content. `OcrEngine::TryCreateFromUserProfileLanguages` is
//! exactly the "use the user's locale" behaviour we want, with no configuration.
//!
//! # Working within the declared bindings
//!
//! This crate enables only the `Media_Ocr`, `Graphics_Imaging` and `Foundation`
//! features of the `windows` crate, and that shapes the implementation in two
//! visible ways:
//!
//! - **No `Storage_Streams`**, so `SoftwareBitmap::CreateCopyFromBuffer` and
//!   friends are unavailable. Pixels go in through `LockBuffer` and
//!   `IMemoryBufferByteAccess` instead, which is the lower-level path but not a
//!   worse one — it is one copy either way.
//! - **No `Globalization`**, so `TryCreateFromLanguage` and the language
//!   enumeration APIs are unavailable. Only the user-profile languages can be
//!   requested. [`Options::languages`](crate::Options::languages) is therefore
//!   advisory here and a mismatch is logged rather than failed, because refusing
//!   to recognise anything would be a much worse outcome than recognising it in
//!   the user's own language.
//!
//! Neither gap is worth a new dependency feature for what it would buy.
//!
//! # Coordinates
//!
//! Windows reports top-left pixel rectangles — no vertical flip — but they are in
//! the coordinate space of the bitmap *handed to the engine*, which is the
//! upscaled one. [`crate::layout::pixels_to_physical`] divides that back out.
//! `OcrLine` carries no rectangle of its own, so a line's bounds are the union
//! of its words'.

use std::ffi::c_void;
use std::ptr;
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID, HRESULT};
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapBufferAccessMode, BitmapPixelFormat, SoftwareBitmap,
};
use windows::Media::Ocr::OcrEngine;
use scrozz_core::{Error, Frame, Result};

use crate::layout;
use crate::prepare::{self, Prepared};
use crate::{Options, TextBlock};

/// `IMemoryBufferByteAccess`, the only way to reach a `SoftwareBitmap`'s pixels
/// without the `Storage_Streams` bindings.
const IMEMORY_BUFFER_BYTE_ACCESS: GUID = GUID::from_u128(0x5b0d_3235_4dba_4d44_865e_8f1d_0e4f_d04d);

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

/// Recognises text in a frame using the Windows OCR engine.
///
/// # Errors
///
/// [`Error::InvalidRequest`] for a malformed frame, [`Error::Unsupported`] if
/// the machine has no OCR language pack installed, [`Error::Platform`] for any
/// other WinRT failure.
pub fn recognize(frame: &Frame, options: &Options) -> Result<Vec<TextBlock>> {
    if !options.languages.is_empty() {
        tracing::debug!(
            requested = ?options.languages,
            "Windows OCR uses the user profile languages; requested languages are advisory"
        );
    }

    // Ask the engine for its ceiling first: the answer feeds the upscale
    // decision, so an image is never enlarged past what the engine will accept
    // and an already-oversized capture is shrunk rather than rejected.
    let max_dimension = OcrEngine::MaxImageDimension().ok();
    let prepared = prepare::prepare(frame, options.upscale, max_dimension)?;

    // A missing engine is a configuration gap the user can close, not a bug, so
    // it gets an Unsupported with the remedy rather than an opaque HRESULT.
    let engine = OcrEngine::TryCreateFromUserProfileLanguages().map_err(|e| {
        Error::Unsupported {
            what: "text recognition".to_string(),
            why: format!(
                "Windows has no OCR language pack for your display languages. \
                 Add one in Settings > Time & language > Language & region > \
                 Add a language, choosing a language whose optional features \
                 include Optical character recognition ({e})"
            ),
        }
    })?;

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

/// Copies a prepared image into a `SoftwareBitmap` the engine can consume.
fn software_bitmap(prepared: &Prepared) -> Result<SoftwareBitmap> {
    let width = i32::try_from(prepared.image.width)
        .map_err(|_| Error::InvalidRequest("image is too wide for Windows OCR".to_string()))?;
    let height = i32::try_from(prepared.image.height)
        .map_err(|_| Error::InvalidRequest("image is too tall for Windows OCR".to_string()))?;

    // Premultiplied BGRA is the format SoftwareBitmap is happiest with and the
    // one the OCR engine accepts without an internal conversion.
    let bitmap = SoftwareBitmap::CreateWithAlpha(
        BitmapPixelFormat::Bgra8,
        width,
        height,
        BitmapAlphaMode::Premultiplied,
    )
    .map_err(|e| Error::Platform(format!("SoftwareBitmap::CreateWithAlpha failed: {e}")))?;

    {
        let buffer = bitmap
            .LockBuffer(BitmapBufferAccessMode::Write)
            .map_err(|e| Error::Platform(format!("SoftwareBitmap::LockBuffer failed: {e}")))?;
        let plane = buffer
            .GetPlaneDescription(0)
            .map_err(|e| Error::Platform(format!("BitmapBuffer::GetPlaneDescription failed: {e}")))?;
        let reference = buffer
            .CreateReference()
            .map_err(|e| Error::Platform(format!("BitmapBuffer::CreateReference failed: {e}")))?;

        let mut raw: *mut c_void = ptr::null_mut();
        // SAFETY: `reference` is a live COM object and `raw` is a valid slot for
        // the returned interface pointer.
        unsafe { reference.query(&IMEMORY_BUFFER_BYTE_ACCESS, &mut raw) }
            .ok()
            .map_err(|e| {
                Error::Platform(format!("IMemoryBufferByteAccess is unavailable: {e}"))
            })?;
        let access = ByteAccess::new(raw).ok_or_else(|| {
            Error::Platform("IMemoryBufferByteAccess query returned null".to_string())
        })?;

        // SAFETY: `access` owns a live IMemoryBufferByteAccess.
        let (dst, capacity) = unsafe { access.buffer() }?;
        copy_rows(&prepared.image.data, dst, capacity, &plane)?;

        // Release the write lock before recognition: the engine cannot read a
        // bitmap that is still locked.
        buffer
            .Close()
            .map_err(|e| Error::Platform(format!("BitmapBuffer::Close failed: {e}")))?;
    }

    Ok(bitmap)
}

/// Writes RGBA rows into a locked BGRA plane, honouring its stride.
fn copy_rows(
    src: &[u8],
    dst: *mut u8,
    capacity: u32,
    plane: &windows::Graphics::Imaging::BitmapPlaneDescription,
) -> Result<()> {
    let width = usize::try_from(plane.Width).unwrap_or(0);
    let height = usize::try_from(plane.Height).unwrap_or(0);
    let stride = usize::try_from(plane.Stride).unwrap_or(0);
    let start = usize::try_from(plane.StartIndex).unwrap_or(0);
    if width == 0 || height == 0 || stride < width * 4 {
        return Err(Error::Platform(format!(
            "SoftwareBitmap reported an unusable plane: {width}x{height} stride {stride}"
        )));
    }
    let needed = start + (height - 1) * stride + width * 4;
    if needed > capacity as usize {
        return Err(Error::Platform(format!(
            "SoftwareBitmap buffer is {capacity} bytes, need {needed}"
        )));
    }
    if src.len() < height * width * 4 {
        return Err(Error::Platform(
            "prepared image is smaller than the bitmap it must fill".to_string(),
        ));
    }

    for y in 0..height {
        let row = &src[y * width * 4..(y + 1) * width * 4];
        // SAFETY: bounds were checked against `capacity` above, and the source
        // row is exactly `width * 4` bytes.
        let out = unsafe { std::slice::from_raw_parts_mut(dst.add(start + y * stride), width * 4) };
        for (s, d) in row
            .as_chunks::<4>()
            .0
            .iter()
            .zip(out.as_chunks_mut::<4>().0.iter_mut())
        {
            let a = s[3];
            // RGBA straight -> BGRA premultiplied, in one pass.
            d[0] = premultiply(s[2], a);
            d[1] = premultiply(s[1], a);
            d[2] = premultiply(s[0], a);
            d[3] = a;
        }
    }
    Ok(())
}

/// Scales a straight-alpha channel by its alpha.
fn premultiply(channel: u8, alpha: u8) -> u8 {
    match alpha {
        255 => channel,
        0 => 0,
        a => ((u32::from(channel) * u32::from(a) + 127) / 255) as u8,
    }
}

/// The `IMemoryBufferByteAccess` vtable: `IUnknown` followed by `GetBuffer`.
#[repr(C)]
struct ByteAccessVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_buffer: unsafe extern "system" fn(*mut c_void, *mut *mut u8, *mut u32) -> HRESULT,
}

/// An owned `IMemoryBufferByteAccess` pointer that releases itself.
///
/// Declared by hand rather than with `#[windows::core::interface]`: that macro
/// emits paths rooted at `::windows_core`, and this crate depends on `windows`
/// rather than on `windows-core` directly, so the macro's paths do not resolve.
/// Four vtable slots is a small price for not adding a dependency.
struct ByteAccess(*mut c_void);

impl ByteAccess {
    /// Takes ownership of a queried interface pointer.
    fn new(raw: *mut c_void) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            Some(Self(raw))
        }
    }

    /// Returns the buffer's base pointer and capacity in bytes.
    ///
    /// # Safety
    ///
    /// The returned pointer is valid only while the owning `BitmapBuffer` lock
    /// is held.
    unsafe fn buffer(&self) -> Result<(*mut u8, u32)> {
        let mut data: *mut u8 = ptr::null_mut();
        let mut capacity: u32 = 0;
        // SAFETY: `self.0` is a live IMemoryBufferByteAccess, so slot 3 of its
        // vtable is `GetBuffer` with this signature.
        let hr = unsafe {
            let vtbl = *(self.0 as *const *const ByteAccessVtbl);
            ((*vtbl).get_buffer)(self.0, &mut data, &mut capacity)
        };
        hr.ok()
            .map_err(|e| Error::Platform(format!("IMemoryBufferByteAccess::GetBuffer failed: {e}")))?;
        if data.is_null() {
            return Err(Error::Platform(
                "IMemoryBufferByteAccess::GetBuffer returned null".to_string(),
            ));
        }
        Ok((data, capacity))
    }
}

impl Drop for ByteAccess {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live COM pointer this type owns exactly one
        // reference to, and slot 2 is `IUnknown::Release`.
        unsafe {
            let vtbl = *(self.0 as *const *const ByteAccessVtbl);
            ((*vtbl).release)(self.0);
        }
    }
}
