//! Text recognition over captured images.
//!
//! # The shape of the problem
//!
//! macOS and packaged Windows builds use the operating system's recogniser:
//! Vision and `Windows.Media.Ocr`, respectively. Portable Windows has no package
//! identity and uses the artifact-local Tesseract payload instead. Linux ships no
//! system engine, so per decision D8 that gap is reported honestly rather than
//! papered over with a silent empty result.
//!
//! # What is actually hard
//!
//! Not calling the engine. The hard parts are the three things around it, and
//! all three are platform-independent — which is why they live in [`prepare`]
//! and [`layout`] where every platform's CI can test them:
//!
//! 1. **Resolution.** A screenshot at 1× is roughly 72 DPI. Both system engines
//!    were tuned on 2× content and both degrade sharply below it. [`prepare`]
//!    upscales before recognition, and on a 1× display this single step is the
//!    difference between usable output and an empty list.
//! 2. **Coordinates.** Vision returns normalised bottom-left-origin rectangles;
//!    Windows returns top-left pixels in the *upscaled* image. Scrozz's UI needs
//!    top-left logical points over the original frame. [`layout`] does both
//!    conversions as pure functions.
//! 3. **Reading order.** Users copy this text and paste it. Output that pastes as
//!    a bag of words is a failed feature even when every glyph is right.
//!
//! # Usage
//!
//! ```no_run
//! use scrozz_ocr::{Ocr, SystemOcr};
//! # fn demo(frame: &scrozz_core::Frame) -> scrozz_core::Result<()> {
//! let blocks = SystemOcr::new().recognize(frame)?;
//! println!("{}", scrozz_ocr::plain_text(&blocks));
//! # Ok(())
//! # }
//! ```

// Platform APIs are reached through objc2 / windows-rs / x11rb, all of which
// require `unsafe`. It is confined to this crate: every crate above it in the
// dependency graph forbids unsafe outright.
#![deny(unsafe_op_in_unsafe_fn)]

use scrozz_core::{Frame, LogicalRect, Result};

/// Windows COM-apartment setup and error mapping.
pub mod apartment;
pub mod layout;
pub mod prepare;

pub use layout::plain_text;
pub use prepare::UpscalePolicy;

/// Absolute Tesseract directory override for source builds and tests.
///
/// This does not select Tesseract: packaged Windows processes always use
/// `Windows.Media.Ocr`. In an unpackaged process, an unset override resolves to
/// `tesseract/` beside `scrozz.exe`. The directory must contain
/// `tesseract.exe`, its dependent DLLs, and `tessdata/eng.traineddata`.
pub const TESSERACT_DIRECTORY_ENV: &str = "SCROZZ_TESSERACT_DIR";

#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "windows", test))]
mod tesseract;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(any(target_os = "windows", test))]
mod windows_runtime;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod unsupported;

/// One recognised span of text.
#[derive(Debug, Clone, PartialEq)]
pub struct TextBlock {
    /// The recognised text.
    pub text: String,
    /// Where it sits in the image.
    pub bounds: LogicalRect,
    /// Engine confidence, 0.0 to 1.0.
    pub confidence: f32,
}

/// Extracts text from images.
///
/// Vision runs on macOS, packaged Windows uses Windows Media OCR, and portable
/// Windows uses its local Tesseract payload. Linux has no configured engine, so
/// per decision D8 that gap is stated plainly rather than hidden.
pub trait Ocr {
    /// Recognises text in a frame.
    ///
    /// # Errors
    ///
    /// Returns [`scrozz_core::Error::Unsupported`] if no engine is available.
    fn recognize(&self, frame: &Frame) -> Result<Vec<TextBlock>>;
}

/// How much time the engine may spend per image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Accuracy {
    /// Best quality. The right default: a screenshot is one small image and a
    /// person is waiting for the answer, so tens of milliseconds of extra work
    /// is invisible while a misread word is not.
    #[default]
    Accurate,
    /// Lower quality, lower latency. Intended for live preview, where results
    /// are recomputed as a selection is dragged.
    Fast,
}

/// Tuning for [`SystemOcr`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Options {
    /// BCP-47 tags in priority order, e.g. `["en-US", "de-DE"]`.
    ///
    /// Empty means "use the languages the user has configured", which is the
    /// right default on both platforms.
    ///
    /// A tag with no installed recogniser is skipped. If *none* of the requested
    /// languages is available, macOS falls back to automatic language detection
    /// and Windows returns [`Error::Unsupported`] naming what is installed —
    /// Windows has no detection mode, and recognising text with the wrong
    /// recogniser yields plausible nonsense rather than a visible failure.
    pub languages: Vec<String>,
    /// Quality/latency trade-off.
    pub accuracy: Accuracy,
    /// Whether to enlarge small images before recognition. Leave on unless the
    /// caller has already prepared the pixels.
    pub upscale: UpscalePolicy,
    /// Whether to apply the engine's language model to fix up unlikely words.
    ///
    /// Off by default, and that is deliberate. Language correction is built for
    /// prose; screenshots are full of identifiers, paths, hashes and version
    /// numbers, and "correcting" `libssl.so.1.1` into English is worse than the
    /// raw read. Turn it on for recognising a photographed document.
    pub language_correction: bool,
}

impl Options {
    /// Default options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the preferred languages.
    #[must_use]
    pub fn with_languages<I, S>(mut self, languages: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.languages = languages.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the accuracy level.
    #[must_use]
    pub const fn with_accuracy(mut self, accuracy: Accuracy) -> Self {
        self.accuracy = accuracy;
        self
    }

    /// Sets the upscale policy.
    #[must_use]
    pub const fn with_upscale(mut self, upscale: UpscalePolicy) -> Self {
        self.upscale = upscale;
        self
    }

    /// Enables or disables the engine's language model.
    #[must_use]
    pub const fn with_language_correction(mut self, enabled: bool) -> Self {
        self.language_correction = enabled;
        self
    }
}

/// The operating system's text recogniser.
///
/// Vision on macOS, `Windows.Media.Ocr` in a packaged Windows process,
/// artifact-local Tesseract in a portable Windows process, and an honest
/// [`scrozz_core::Error::Unsupported`] elsewhere.
///
/// # Extending to Linux
///
/// [`Ocr`] is a trait precisely so this type is not the only answer. A Tesseract
/// or ONNX backend belongs behind an optional Cargo feature in this crate,
/// implementing [`Ocr`] and reusing [`prepare`] and [`layout`] verbatim — those
/// two modules are the majority of the work and are already platform-independent
/// and tested. Nothing is bundled by default: a screenshot tool that silently
/// grows by an 80 MB language model has made a decision on the user's behalf
/// that it had no right to make.
#[derive(Debug, Clone, Default)]
pub struct SystemOcr {
    options: Options,
}

impl SystemOcr {
    /// Creates a recogniser with default options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a recogniser with explicit options.
    #[must_use]
    pub const fn with_options(options: Options) -> Self {
        Self { options }
    }

    /// The options in force.
    #[must_use]
    pub const fn options(&self) -> &Options {
        &self.options
    }

    /// Whether this build has a working engine.
    ///
    /// Lets a UI hide or disable the command up front instead of offering
    /// something that can only fail.
    #[must_use]
    pub const fn is_available() -> bool {
        cfg!(any(target_os = "macos", target_os = "windows"))
    }

    /// Stable backend token suitable for diagnostics and machine output.
    ///
    /// # Errors
    ///
    /// On Windows, returns a platform error if process package identity cannot be
    /// determined. Package identity is the only input to backend selection.
    pub fn engine_name() -> Result<&'static str> {
        #[cfg(target_os = "macos")]
        {
            Ok("vision")
        }
        #[cfg(target_os = "windows")]
        {
            windows::engine_name()
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Ok("unavailable")
        }
    }

    /// Recognises text and returns it as lines, ready to copy.
    ///
    /// # Errors
    ///
    /// As [`Ocr::recognize`].
    pub fn recognize_text(&self, frame: &Frame) -> Result<String> {
        Ok(plain_text(&self.recognize(frame)?))
    }
}

impl Ocr for SystemOcr {
    fn recognize(&self, frame: &Frame) -> Result<Vec<TextBlock>> {
        #[cfg(target_os = "macos")]
        {
            macos::recognize(frame, &self.options)
        }
        #[cfg(target_os = "windows")]
        {
            windows::recognize(frame, &self.options)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            unsupported::recognize(frame, &self.options)
        }
    }
}
