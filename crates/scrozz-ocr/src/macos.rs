//! macOS text recognition via Vision.
//!
//! # Why Vision
//!
//! `VNRecognizeTextRequest` is the same engine Preview and Live Text use. It is
//! strong on exactly the content screenshots contain — small antialiased UI text,
//! light-on-dark themes, mixed fonts — it is already localised to whatever
//! languages the user has, and it costs nothing in binary size. A bundled model
//! would be larger, worse and out of date.
//!
//! # The two things that go wrong here
//!
//! **Coordinates.** Vision reports `boundingBox` as a normalised rectangle whose
//! origin is at the image's **bottom-left**. Scrozz uses top-left logical points.
//! The flip lives in [`crate::layout::bottom_left_normalized_to_physical`], on
//! its own, where a test can pin it — because a vertical flip produces perfectly
//! plausible-looking boxes and hides easily in a smoke test.
//!
//! **Languages.** Setting an unsupported language tag makes `performRequests`
//! fail outright rather than degrade, so the user's preferred languages are
//! filtered against the request's own supported list before being applied. A
//! user with `["en-GB", "cy"]` should not lose OCR entirely because Vision has
//! no Welsh.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
};
use objc2_foundation::{NSArray, NSDictionary, NSLocale, NSString};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
};
use scrozz_core::{Error, Frame, Result};

use crate::layout::{self, NormalizedRect};
use crate::prepare::{self, Prepared};
use crate::{Accuracy, Options, TextBlock};

/// Recognises text in a frame using Vision.
///
/// # Errors
///
/// [`Error::InvalidRequest`] for a malformed frame, [`Error::Platform`] if any
/// Core Graphics or Vision call fails.
pub fn recognize(frame: &Frame, options: &Options) -> Result<Vec<TextBlock>> {
    // Vision has no documented maximum image size, so no ceiling is imposed
    // beyond the resampler's own pixel budget.
    let prepared = prepare::prepare(frame, options.upscale, None)?;
    let image = cg_image(&prepared)?;

    let request = VNRecognizeTextRequest::new();
    configure(&request, options);

    // SAFETY: `image` is a live CGImage and the dictionary is a valid, empty
    // NSDictionary of the expected key/value types.
    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            &image,
            &NSDictionary::new(),
        )
    };

    let requests: Retained<NSArray<VNRequest>> = NSArray::from_slice(&[request.as_ref()]);
    handler
        .performRequests_error(&requests)
        .map_err(|e| Error::Platform(format!("Vision text recognition failed: {e}")))?;

    let Some(observations) = request.results() else {
        // No results object at all is not an error — an image with no text is a
        // perfectly ordinary outcome and must not look like a failure.
        return Ok(Vec::new());
    };

    let source = prepared.source_size;
    let mut blocks = Vec::with_capacity(observations.len());
    for observation in &observations {
        // One candidate: the alternatives are for interactive correction UI,
        // which Scrozz does not have, and asking for more costs time.
        let candidates = observation.topCandidates(1);
        let Some(best) = candidates.firstObject() else {
            continue;
        };

        let text = best.string().to_string();
        if text.is_empty() {
            continue;
        }

        // SAFETY: `observation` is a live VNRecognizedTextObservation.
        let bb = unsafe { observation.boundingBox() };
        let rect = NormalizedRect::new(
            bb.origin.x,
            bb.origin.y,
            bb.size.width,
            bb.size.height,
        );
        let bounds = layout::bottom_left_normalized_to_physical(rect, source);
        if bounds.is_empty() {
            continue;
        }

        blocks.push(TextBlock {
            text,
            bounds: layout::to_logical(bounds, frame.scale),
            // A real per-observation number, not a placeholder. The UI uses it
            // to decide what to surface, so a constant here would quietly
            // disable that.
            confidence: best.confidence().clamp(0.0, 1.0),
        });
    }

    Ok(layout::sort_reading_order(blocks))
}

/// Applies options to a freshly created request.
fn configure(request: &VNRecognizeTextRequest, options: &Options) {
    request.setRecognitionLevel(match options.accuracy {
        Accuracy::Accurate => VNRequestTextRecognitionLevel::Accurate,
        Accuracy::Fast => VNRequestTextRecognitionLevel::Fast,
    });
    request.setUsesLanguageCorrection(options.language_correction);

    // Vision documents a default minimum text height as a *fraction of image
    // height*. Measured on current macOS it does not appear to filter 18pt text
    // in a 1200px-tall capture, but the documented behaviour would discard
    // exactly the menu-bar and status-line text a screenshot tool cares about,
    // and the threshold has changed between OS releases. Zero means "no
    // minimum", which is the behaviour to pin rather than inherit.
    request.setMinimumTextHeight(0.0);

    let languages = supported_languages(request, &options.languages);
    if languages.is_empty() {
        // Nothing usable — let Vision pick. Better a default language than a
        // hard failure from an empty language list.
        request.setAutomaticallyDetectsLanguage(true);
        return;
    }

    let strings: Vec<Retained<NSString>> = languages.iter().map(|s| NSString::from_str(s)).collect();
    request.setRecognitionLanguages(&NSArray::from_retained_slice(&strings));
    request.setAutomaticallyDetectsLanguage(false);
}

/// Picks the language tags to request, keeping only ones Vision accepts.
///
/// Falls back to the user's system preferences when the caller named none, which
/// is the behaviour a screenshot tool wants by default: whatever languages the
/// person actually reads.
fn supported_languages(request: &VNRecognizeTextRequest, requested: &[String]) -> Vec<String> {
    // SAFETY: `request` is live and fully configured; this reads a property.
    let Ok(supported) = (unsafe { request.supportedRecognitionLanguagesAndReturnError() }) else {
        // Cannot tell what is supported, so pass nothing through and let Vision
        // use its own default rather than risk an invalid tag.
        return Vec::new();
    };
    let supported: Vec<String> = supported.iter().map(|s| s.to_string()).collect();
    if supported.is_empty() {
        return Vec::new();
    }

    let preferred: Vec<String> = if requested.is_empty() {
        NSLocale::preferredLanguages()
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        requested.to_vec()
    };

    let mut out = Vec::new();
    for tag in preferred {
        // Match "en-GB" against a supported "en-GB", else against "en". Vision
        // enumerates specific tags, and a user's locale is often more specific
        // than anything on the list.
        let base = tag.split(['-', '_']).next().unwrap_or(&tag).to_string();
        let hit = supported
            .iter()
            .find(|s| s.eq_ignore_ascii_case(&tag))
            .or_else(|| {
                supported.iter().find(|s| {
                    s.split(['-', '_'])
                        .next()
                        .is_some_and(|b| b.eq_ignore_ascii_case(&base))
                })
            });
        if let Some(hit) = hit
            && !out.iter().any(|existing: &String| existing == hit) {
                out.push(hit.clone());
            }
    }
    out
}

/// Wraps a prepared image in a `CGImage` Vision can read.
///
/// Returns an opaque owner rather than a named handle: Core Graphics types are
/// reference-counted through `CFRetained`, and `objc2-core-foundation` is not a
/// declared dependency of this crate, so the type is deliberately not spelled.
fn cg_image(prepared: &Prepared) -> Result<impl std::ops::Deref<Target = CGImage>> {
    let (width, height) = (prepared.image.width as usize, prepared.image.height as usize);
    let bytes_per_row = width * 4;
    let data = prepared.image.data.clone();
    let len = data.len();

    // The provider must own the bytes: Core Graphics may hold them well past
    // this function, and Vision's work happens after `performRequests` starts.
    // Leak the Vec here and reclaim it in the release callback.
    let boxed = Box::into_raw(data.into_boxed_slice());
    let ptr = boxed.cast::<u8>();

    // SAFETY: `ptr` is a live allocation of exactly `len` bytes, kept alive
    // until `release_boxed_slice` runs, and `info` carries the length needed to
    // reconstitute the box.
    let provider = unsafe {
        CGDataProvider::with_data(
            len as *mut c_void,
            ptr.cast::<c_void>(),
            len,
            Some(release_boxed_slice),
        )
    };
    let Some(provider) = provider else {
        // Nothing took ownership, so reclaim the leak rather than leave it.
        // SAFETY: `boxed` came from `Box::into_raw` and has not been freed.
        drop(unsafe { Box::from_raw(boxed) });
        return Err(Error::Platform(
            "CGDataProviderCreateWithData returned null".to_string(),
        ));
    };

    let color_space = CGColorSpace::new_device_rgb()
        .ok_or_else(|| Error::Platform("CGColorSpaceCreateDeviceRGB returned null".to_string()))?;

    // SAFETY: geometry matches the buffer exactly; `decode` is null, which means
    // "default decode array"; the provider and colour space are live.
    let image = unsafe {
        CGImage::new(
            width,
            height,
            8,
            32,
            bytes_per_row,
            Some(&color_space),
            // Straight (non-premultiplied) alpha in the last byte: RGBA, which
            // is what `prepare` guarantees.
            CGBitmapInfo::from_bits_retain(CGImageAlphaInfo::Last.0),
            Some(&provider),
            ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    };

    image.ok_or_else(|| Error::Platform("CGImageCreate returned null".to_string()))
}

/// Frees the buffer handed to `CGDataProviderCreateWithData`.
///
/// # Safety
///
/// `data` must be the pointer produced by `Box::into_raw` on a `Box<[u8]>`, and
/// `info` must be that slice's length in bytes.
unsafe extern "C-unwind" fn release_boxed_slice(info: *mut c_void, data: NonNull<c_void>, _size: usize) {
    let len = info as usize;
    let slice = ptr::slice_from_raw_parts_mut(data.as_ptr().cast::<u8>(), len);
    // SAFETY: by this function's contract `slice` is the original boxed slice,
    // and Core Graphics calls this exactly once when the provider dies.
    drop(unsafe { Box::from_raw(slice) });
}
