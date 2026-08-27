//! Text recognition over captured images.

#![forbid(unsafe_code)]

use scrozz_core::{Frame, LogicalRect, Result};

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
/// Every platform ships a competent engine — Vision on macOS, Windows OCR on
/// Windows — and both are far better than a bundled model at the sizes and
/// fonts screenshots actually contain. Linux has no system engine, so it needs
/// Tesseract or an ONNX model; per decision D8 that gap is stated plainly rather
/// than hidden.
pub trait Ocr {
    /// Recognises text in a frame.
    ///
    /// # Errors
    ///
    /// Returns [`scrozz_core::Error::Unsupported`] if no engine is available.
    fn recognize(&self, frame: &Frame) -> Result<Vec<TextBlock>>;
}
