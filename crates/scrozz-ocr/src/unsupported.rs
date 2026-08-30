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
//! - **Silently shell out to `tesseract` if it happens to exist.** Unpredictable
//!   unless it is an explicit build capability with discoverable language packs
//!   and package-aware errors. The default `tesseract` feature provides exactly
//!   that contract on Linux.
//! - **Return an empty `Vec`.** The worst option. Indistinguishable from "this
//!   image contains no text", so the user concludes the feature is broken and
//!   has no idea why.
//!
//! # The recommended path
//!
//! Build Linux with the default `tesseract` feature and install the distro's
//! Tesseract executable plus language data. Scrozz streams a portable image to
//! the subprocess, so the Rust build remains free of C libraries and bindgen.

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

/// Reports that this build has no OCR engine capable of listing languages.
pub fn available_languages() -> Result<Vec<String>> {
    Err(Error::Unsupported {
        what: "OCR language listing".to_string(),
        why: "this build has no OCR engine. On Linux, install Tesseract and use \
              Scrozz's default `tesseract` feature"
            .to_string(),
    })
}
