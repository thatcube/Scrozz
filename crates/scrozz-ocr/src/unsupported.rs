//! The honest answer on platforms with no system text recogniser.
//!
//! # Why this is a real backend and not a `todo!()`
//!
//! macOS and Windows both ship a text recogniser in the OS. Linux does not —
//! there is no freedesktop, X11 or Wayland protocol for it, and no library every
//! distribution can be assumed to have. Decision D8 says a gap like this gets
//! named, not hidden, so this module returns a
//! [`Error::Unsupported`](scrozz_core::Error::Unsupported) whose `why` tells the
//! user exactly which package would fix it.
//!
//! The alternatives were all worse:
//!
//! - **Bundle a model.** Tesseract's English data is ~15 MB; a modern ONNX
//!   detector/recogniser pair is far more. Multiplying a screenshot tool's
//!   download for a feature most users never open is a decision to make loudly,
//!   if at all — not quietly in a dependency list.
//! - **Shell out to `tesseract` if it happens to exist.** Unpredictable: results
//!   depend on which language packs are installed and which of two very
//!   different engine versions is present, and the failure mode is bad text
//!   rather than a clear message.
//! - **Return an empty `Vec`.** The worst option. Indistinguishable from "this
//!   image contains no text", so the user concludes the feature is broken and
//!   has no idea why.
//!
//! # The recommended path
//!
//! Add an optional `tesseract` feature to this crate gated on the `leptonica-
//! plumbing`/`tesseract` bindings, implementing [`Ocr`](crate::Ocr) exactly as
//! the other backends do. It needs no new abstraction: [`crate::prepare`] hands
//! it a tightly packed RGBA8 buffer that maps straight onto a Leptonica `PIX`,
//! and [`crate::layout`] already turns top-left pixel rectangles into logical
//! ones with [`pixels_to_physical`](crate::layout::pixels_to_physical). The
//! platform-specific part is a few dozen lines; everything expensive is shared
//! and already tested on every target.

use scrozz_core::{Error, Frame, Result};

use crate::{Options, TextBlock};

/// Reports that no engine is available, naming what would provide one.
///
/// # Errors
///
/// Always returns [`Error::Unsupported`].
pub fn recognize(_frame: &Frame, _options: &Options) -> Result<Vec<TextBlock>> {
    Err(Error::Unsupported {
        what: "text recognition".to_string(),
        why: "this platform ships no system OCR engine (macOS uses Vision, \
              Windows uses Windows.Media.Ocr). Install Tesseract — \
              `apt install tesseract-ocr tesseract-ocr-eng`, \
              `dnf install tesseract tesseract-langpack-eng`, or \
              `pacman -S tesseract tesseract-data-eng` — and rebuild Scrozz \
              with `--features tesseract`. Scrozz does not bundle a language \
              model, so the download stays small for the majority who never \
              use this feature"
            .to_string(),
    })
}

/// Reports that this build has no OCR language inventory.
///
/// # Errors
///
/// Always returns [`Error::Unsupported`].
pub fn available_languages() -> Result<Vec<String>> {
    Err(Error::Unsupported {
        what: "listing OCR languages".to_string(),
        why: "this platform has no configured OCR engine".to_string(),
    })
}
