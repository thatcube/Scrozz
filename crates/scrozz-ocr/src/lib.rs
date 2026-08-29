//! Text recognition over captured images.
//!
//! # The shape of the problem
//!
//! macOS and packaged Windows builds use the operating system's recogniser:
//! Vision and `Windows.Media.Ocr`, respectively. The portable Windows ZIP has no
//! package identity and therefore uses the same timeout-bounded local Tesseract
//! subprocess as Linux. None of these paths sends image content off the machine.
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

use scrozz_core::{Error, Frame, LogicalRect, Result};

pub mod barcode;
mod config;
pub mod layout;
pub mod links;
mod live;
pub mod prepare;
pub mod sensitive;

pub use barcode::{
    Barcode, BarcodeDetector, BarcodeOptions, PortableBarcodes, Symbology, SystemBarcodes,
};
pub use config::{
    AUTO_DETECT_LANGUAGE_KEY, DETECT_LINKS_KEY, KEEP_LINE_BREAKS_KEY, LANGUAGES_KEY, LanguageMode,
    RuntimeConfig,
};
pub use layout::{LineBreaks, plain_text, text};
pub use links::{Link, LinkKind, links};
pub use live::{LiveOcr, block_at_point, frame_local_point};
pub use prepare::UpscalePolicy;
pub use sensitive::{
    CancellationToken, FindingConfidence, FindingId, FindingReason, LocalSensitiveDetector,
    SensitiveCategory, SensitiveFinding, SensitiveScan, SensitiveScanCache, SensitiveScanOptions,
    SensitiveSource,
};

/// Optional absolute directory containing a local Tesseract installation.
///
/// Portable Windows expects `tesseract.exe` and `tessdata/*.traineddata` below
/// this directory. Linux expects `tesseract` with the same `tessdata` layout.
/// When unset, portable Windows uses `tesseract/` beside `scrozz.exe`; Linux
/// resolves `tesseract` through `PATH`.
pub const TESSERACT_DIRECTORY_ENV: &str = "SCROZZ_TESSERACT_DIR";

#[cfg(target_os = "macos")]
mod macos;
#[cfg(all(
    any(target_os = "linux", target_os = "windows", test),
    feature = "tesseract"
))]
mod tesseract;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    all(target_os = "linux", feature = "tesseract")
)))]
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
/// Vision runs on macOS, Windows OCR runs when the process has package identity,
/// and Tesseract runs on Linux and in the unpackaged Windows artifact.
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
    /// Empty means "resolve the languages the user has configured".
    ///
    /// A tag with no installed recogniser is skipped. If *none* of the requested
    /// languages is available, the backend returns
    /// [`Error::Unsupported`] naming what is installed. Recognising text with
    /// the wrong model yields plausible nonsense rather than a visible failure.
    pub languages: Vec<String>,
    /// Ask a capable backend to infer the language from the image.
    ///
    /// This differs from an empty [`Self::languages`] list, which means to use
    /// the person's configured system languages.
    pub automatic_language_detection: bool,
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
    /// How visual OCR lines are joined for plain-text output.
    pub line_breaks: LineBreaks,
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

    /// Enables or disables explicit automatic language detection.
    #[must_use]
    pub const fn with_automatic_language_detection(mut self, enabled: bool) -> Self {
        self.automatic_language_detection = enabled;
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

    /// Sets how visual lines become plain text.
    #[must_use]
    pub const fn with_line_breaks(mut self, line_breaks: LineBreaks) -> Self {
        self.line_breaks = line_breaks;
        self
    }
}

/// The operating system's text recogniser.
///
/// Vision on macOS, `Windows.Media.Ocr` in a packaged Windows process, and the
/// locally installed `tesseract` executable on Linux or portable Windows when
/// the default `tesseract` feature is enabled. The subprocess integration links
/// no native library; artifact packaging decides whether to bundle its executable
/// and models. A missing executable or language package is returned as an
/// actionable [`scrozz_core::Error::Unsupported`].
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

    /// Replaces the options used by subsequent recognition calls.
    pub fn set_options(&mut self, options: Options) {
        self.options = options;
    }

    /// Whether this build contains an OCR engine integration.
    ///
    /// This is the original const capability API. Call
    /// [`Self::is_runtime_available`] when installed language models and the
    /// selected Tesseract runtime must be checked.
    #[must_use]
    pub const fn is_available() -> bool {
        cfg!(any(
            target_os = "macos",
            target_os = "windows",
            all(target_os = "linux", feature = "tesseract")
        ))
    }

    /// Whether the engine selected for this process is currently usable.
    ///
    /// Linux and portable Windows probe the exact Tesseract runtime selected by
    /// `PATH` or [`TESSERACT_DIRECTORY_ENV`] on every call, so an environment or
    /// installation change cannot leave a stale positive answer.
    #[must_use]
    pub fn is_runtime_available() -> bool {
        #[cfg(target_os = "macos")]
        {
            true
        }
        #[cfg(target_os = "windows")]
        {
            windows::engine_name().is_ok_and(|engine| match engine {
                "windows-media-ocr" => {
                    windows::available_languages().is_ok_and(|languages| !languages.is_empty())
                }
                "tesseract" => {
                    #[cfg(feature = "tesseract")]
                    {
                        tesseract::is_available()
                    }
                    #[cfg(not(feature = "tesseract"))]
                    {
                        false
                    }
                }
                _ => false,
            })
        }
        #[cfg(all(target_os = "linux", feature = "tesseract"))]
        {
            tesseract::is_available()
        }
        #[cfg(not(any(
            target_os = "macos",
            target_os = "windows",
            all(target_os = "linux", feature = "tesseract")
        )))]
        {
            false
        }
    }

    /// Backend name suitable for diagnostics and machine output.
    ///
    /// # Errors
    ///
    /// On Windows, returns a platform error if package identity cannot be
    /// determined. The packaged artifact selects Windows Media OCR; the portable
    /// artifact selects the local Tesseract subprocess integration.
    pub fn engine_name() -> Result<&'static str> {
        #[cfg(target_os = "macos")]
        {
            Ok("vision")
        }
        #[cfg(target_os = "windows")]
        {
            windows::engine_name()
        }
        #[cfg(all(target_os = "linux", feature = "tesseract"))]
        {
            Ok("tesseract")
        }
        #[cfg(not(any(
            target_os = "macos",
            target_os = "windows",
            all(target_os = "linux", feature = "tesseract")
        )))]
        {
            Ok("unavailable")
        }
    }

    /// Whether this backend can infer language from arbitrary image content.
    #[must_use]
    pub fn supports_automatic_language_detection() -> bool {
        #[cfg(target_os = "macos")]
        {
            macos::supports_automatic_language_detection()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    /// Lists language tags accepted by this backend on this machine.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when no engine is installed and
    /// [`Error::Platform`] when a native query fails.
    pub fn available_languages(&self) -> Result<Vec<String>> {
        #[cfg(target_os = "macos")]
        {
            macos::available_languages(self.options.accuracy)
        }
        #[cfg(target_os = "windows")]
        {
            windows::available_languages()
        }
        #[cfg(all(target_os = "linux", feature = "tesseract"))]
        {
            tesseract::available_languages()
        }
        #[cfg(not(any(
            target_os = "macos",
            target_os = "windows",
            all(target_os = "linux", feature = "tesseract")
        )))]
        {
            unsupported::available_languages()
        }
    }

    /// Recognises text and returns it as lines, ready to copy.
    ///
    /// # Errors
    ///
    /// As [`Ocr::recognize`].
    pub fn recognize_text(&self, frame: &Frame) -> Result<String> {
        Ok(text(&self.recognize(frame)?, self.options.line_breaks))
    }
}

impl Ocr for SystemOcr {
    fn recognize(&self, frame: &Frame) -> Result<Vec<TextBlock>> {
        if self.options.automatic_language_detection && !self.options.languages.is_empty() {
            return Err(Error::InvalidRequest(
                "automatic language detection cannot be combined with preferred languages"
                    .to_string(),
            ));
        }
        #[cfg(target_os = "macos")]
        {
            macos::recognize(frame, &self.options)
        }
        #[cfg(target_os = "windows")]
        {
            windows::recognize(frame, &self.options)
        }
        #[cfg(all(target_os = "linux", feature = "tesseract"))]
        {
            tesseract::recognize(frame, &self.options)
        }
        #[cfg(not(any(
            target_os = "macos",
            target_os = "windows",
            all(target_os = "linux", feature = "tesseract")
        )))]
        {
            unsupported::recognize(frame, &self.options)
        }
    }
}
