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
//!
//! [`Document::source`] is therefore never mutated. Rendering copies it first,
//! and every destructive operation — redaction in particular — acts on that
//! copy. The exported pixels are permanent; the document that produced them is
//! not.
//!
//! # The two rules this crate exists to enforce
//!
//! **Redactions destroy pixels.** [`Annotation::Redact`] is rasterised into the
//! image, never drawn as a shape over intact content. Shipping a blur as an
//! overlay leaves the original underneath it for anyone to recover, which is a
//! real privacy failure that has burned other tools publicly. See
//! [`render::redact`].
//!
//! **Window captures are sacred.** Decision D9: the OS already supplied a
//! window's true shape and shadow, so synthesising corners, padding or a shadow
//! on top yields a subtly wrong image. [`Document::may_beautify`] reports it,
//! [`Document::set_beautification`] refuses it, and
//! [`render::SkiaRenderer::render_at`] refuses it again so no other route into a
//! document can get it wrong.
//!
//! # Coordinates
//!
//! Annotations are authored in *logical* points relative to the source image, so
//! one document renders correctly at 1×, at 2×, and at any export size. The
//! renderer converts to physical pixels once, in path space, so stroke widths
//! and arrowheads scale by exactly the same rule as the geometry they belong to.
//!
//! # Example
//!
//! ```no_run
//! use scrozz_annotate::{Annotation, Document, Renderer, SkiaRenderer, Style};
//! use scrozz_core::{Capture, LogicalPoint};
//!
//! # fn demo(capture: Capture) -> scrozz_core::Result<()> {
//! let mut document = Document::new(capture);
//! document.add(
//!     Annotation::Arrow {
//!         from: LogicalPoint::new(10.0, 10.0),
//!         to: LogicalPoint::new(120.0, 90.0),
//!     },
//!     Style::stroked(),
//! ).expect("annotation id space available");
//!
//! // Persisted invisibly alongside the capture — no project file, D14.
//! let saved = serde_json::to_string(&document.data()).unwrap();
//!
//! let frame = SkiaRenderer::new().render(&document)?;
//! # let _ = (saved, frame);
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

pub mod annotation;
pub mod document;
pub mod font;
pub mod geom;
pub mod render;
pub mod style;

pub use annotation::{Annotation, AnnotationId, AnnotationKind, AnnotationObject, RedactStyle};
pub use document::{
    Alignment, AnnotationMut, AspectPreset, Background, BackgroundImage, Beautification,
    BeautificationPreset, BuiltInBackground, Canvas, CanvasGeometry, CanvasRotation, Document,
    DocumentData, UndoHistory,
};
pub use render::{PhysicalInsets, RenderGeometry, RenderedFrame, Renderer, SkiaRenderer};
pub use style::{ArrowStyle, Color, Style, TextPreset};
