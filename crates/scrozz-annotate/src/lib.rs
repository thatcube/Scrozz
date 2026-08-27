//! The annotation document, its renderer, and beautification.
//!
//! # Annotations are never permanent
//!
//! Decision D14: every annotation stays editable forever, and there is no
//! user-facing project file. The document below is the whole mechanism — a list
//! of vector objects over an untouched source image, persisted invisibly
//! alongside it. Reopening any capture restores every arrow exactly as it was,
//! months later, with no "save as .scrozz" step and nothing for the user to
//! manage or lose. Flattening happens only on export, and never in place.

#![forbid(unsafe_code)]

use scrozz_core::{Capture, Frame, LogicalPoint, LogicalRect, Result};
use serde::{Deserialize, Serialize};

/// One editable annotation.
///
/// Coordinates are logical and relative to the source image, so a document
/// survives being re-rendered at any scale or export size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Annotation {
    /// An arrow from tail to head.
    Arrow {
        /// Tail.
        from: LogicalPoint,
        /// Head.
        to: LogicalPoint,
    },
    /// A rectangle outline.
    Rectangle(LogicalRect),
    /// An ellipse inscribed in a rectangle.
    Ellipse(LogicalRect),
    /// Freehand ink.
    Freehand(Vec<LogicalPoint>),
    /// A text label.
    Text {
        /// Where the text is anchored.
        at: LogicalPoint,
        /// The text itself.
        content: String,
    },
    /// An auto-incrementing numbered step marker.
    Counter {
        /// Where the marker sits.
        at: LogicalPoint,
        /// Its number, assigned in insertion order.
        index: u32,
    },
    /// A translucent highlight.
    Highlight(LogicalRect),
    /// An obscured region.
    ///
    /// Must be applied destructively on export. Exporting a blur as a
    /// *renderable object over intact pixels* would ship the original underneath
    /// the redaction, which is a genuine privacy failure and has burned other
    /// tools publicly.
    Redact {
        /// The region to obscure.
        area: LogicalRect,
        /// How to obscure it.
        style: RedactStyle,
    },
}

/// How a redaction obscures its region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RedactStyle {
    /// Gaussian blur.
    Blur,
    /// Mosaic.
    Pixelate,
    /// Solid fill.
    Solid,
}

/// Padding, background and framing applied around a capture.
///
/// Per decision D9 this is refused outright for window captures: the OS already
/// supplied the window's true shape and shadow, and synthesising them again
/// yields a subtly, unmistakably wrong image. [`Document::may_beautify`] is the
/// enforcement point.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Beautification {
    /// Padding around the image, in logical points.
    pub padding: f64,
    /// Corner radius applied to the image.
    pub corner_radius: f64,
    /// Drop shadow depth.
    pub shadow: f64,
}

/// A capture plus every edit ever made to it.
#[derive(Debug, Clone)]
pub struct Document {
    /// The untouched source. Never mutated.
    pub source: Capture,
    /// Edits, in z-order.
    pub annotations: Vec<Annotation>,
    /// Framing, if permitted.
    pub beautification: Option<Beautification>,
}

impl Document {
    /// Wraps a fresh capture in an empty document.
    #[must_use]
    pub fn new(source: Capture) -> Self {
        Self {
            source,
            annotations: Vec::new(),
            beautification: None,
        }
    }

    /// Whether framing may be applied at all.
    ///
    /// False for window captures. The UI disables the controls entirely rather
    /// than letting them be set and quietly ignored.
    #[must_use]
    pub fn may_beautify(&self) -> bool {
        !self.source.provenance.forbids_compositing()
    }
}

/// Renders a document to pixels.
pub trait Renderer {
    /// Composites annotations and framing over the source.
    ///
    /// # Errors
    ///
    /// Returns an error if rendering failed.
    fn render(&self, document: &Document) -> Result<Frame>;
}
