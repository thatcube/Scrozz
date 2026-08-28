//! Editable annotation objects.
//!
//! Per decision D14 an annotation is never flattened into the image: it stays a
//! live object with an identity, a style and editable geometry for as long as
//! the capture exists. Everything needed to re-edit it months later lives here.

use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};
use serde::{Deserialize, Serialize};

use crate::{font, geom, style::Style};

/// One editable annotation.
///
/// Coordinates are logical and relative to the source image, so a document
/// survives being re-rendered at any scale or export size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Annotation {
    /// An arrow from tail to head.
    Arrow {
        /// Tail.
        from: LogicalPoint,
        /// Head.
        to: LogicalPoint,
    },
    /// A plain straight line, with no head at either end.
    ///
    /// Distinct from a headless [`Self::Arrow`] rather than a special case of
    /// one: an arrow points, and a line connects or underlines. Collapsing the
    /// two would mean a line silently grew a head the moment its stroke width
    /// changed, because head size is derived from stroke width.
    Line {
        /// One end.
        from: LogicalPoint,
        /// The other.
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
        /// Where the text is anchored — the top-left of its first line.
        at: LogicalPoint,
        /// The text itself.
        content: String,
    },
    /// An auto-incrementing numbered step marker.
    ///
    /// `index` is owned by the document, which keeps the sequence contiguous as
    /// markers are added and removed. It is never set directly by a caller.
    Counter {
        /// Where the marker sits — its centre.
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
#[serde(rename_all = "snake_case")]
pub enum RedactStyle {
    /// Gaussian blur.
    Blur,
    /// Mosaic.
    Pixelate,
    /// Solid fill.
    Solid,
}

/// What kind of annotation this is, without its geometry.
///
/// Lets a toolbar, a hit-test filter or a serialiser reason about kinds without
/// matching a non-exhaustive enum with payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AnnotationKind {
    /// [`Annotation::Arrow`].
    Arrow,
    /// [`Annotation::Line`].
    Line,
    /// [`Annotation::Rectangle`].
    Rectangle,
    /// [`Annotation::Ellipse`].
    Ellipse,
    /// [`Annotation::Freehand`].
    Freehand,
    /// [`Annotation::Text`].
    Text,
    /// [`Annotation::Counter`].
    Counter,
    /// [`Annotation::Highlight`].
    Highlight,
    /// [`Annotation::Redact`].
    Redact,
}

impl Annotation {
    /// Which kind of annotation this is.
    #[must_use]
    pub const fn kind(&self) -> AnnotationKind {
        match self {
            Self::Arrow { .. } => AnnotationKind::Arrow,
            Self::Line { .. } => AnnotationKind::Line,
            Self::Rectangle(_) => AnnotationKind::Rectangle,
            Self::Ellipse(_) => AnnotationKind::Ellipse,
            Self::Freehand(_) => AnnotationKind::Freehand,
            Self::Text { .. } => AnnotationKind::Text,
            Self::Counter { .. } => AnnotationKind::Counter,
            Self::Highlight(_) => AnnotationKind::Highlight,
            Self::Redact { .. } => AnnotationKind::Redact,
        }
    }

    /// Whether this annotation destroys the pixels beneath it on render.
    ///
    /// The renderer branches on this rather than on the variant, so a future
    /// redaction-like tool cannot be added without deciding the question.
    #[must_use]
    pub const fn is_destructive(&self) -> bool {
        matches!(self, Self::Redact { .. })
    }

    /// The annotation's geometric extent, ignoring stroke width.
    ///
    /// Text and counters need their style to be measured, so this is the
    /// geometry-only box; use [`AnnotationObject::bounds`] for the visual one.
    #[must_use]
    pub fn bounds(&self) -> LogicalRect {
        match self {
            Self::Arrow { from, to } | Self::Line { from, to } => {
                LogicalRect::from_corners(*from, *to)
            }
            Self::Rectangle(r)
            | Self::Ellipse(r)
            | Self::Highlight(r)
            | Self::Redact { area: r, .. } => *r,
            Self::Freehand(points) => geom::bounding_box(points),
            Self::Text { at, .. } | Self::Counter { at, .. } => {
                LogicalRect::new(*at, LogicalSize::new(0.0, 0.0))
            }
        }
    }

    /// Moves the annotation by `dx`, `dy` logical points.
    pub fn translate(&mut self, dx: f64, dy: f64) {
        let shift = |p: &mut LogicalPoint| {
            p.x += dx;
            p.y += dy;
        };
        match self {
            Self::Arrow { from, to } | Self::Line { from, to } => {
                shift(from);
                shift(to);
            }
            Self::Rectangle(r)
            | Self::Ellipse(r)
            | Self::Highlight(r)
            | Self::Redact { area: r, .. } => {
                shift(&mut r.origin);
            }
            Self::Freehand(points) => points.iter_mut().for_each(shift),
            Self::Text { at, .. } | Self::Counter { at, .. } => shift(at),
        }
    }

    /// Reshapes the annotation to fill `to`.
    ///
    /// Point-based annotations are remapped proportionally, so an arrow keeps
    /// its direction and freehand ink keeps its shape. Anchored annotations —
    /// text and counters — move to the new box rather than stretching, because
    /// their size comes from their type size, not from a drag handle.
    pub fn set_bounds(&mut self, to: LogicalRect) {
        let from = self.bounds();
        match self {
            Self::Arrow { from: a, to: b } | Self::Line { from: a, to: b } => {
                *a = geom::remap(*a, &from, &to);
                *b = geom::remap(*b, &from, &to);
            }
            Self::Rectangle(r)
            | Self::Ellipse(r)
            | Self::Highlight(r)
            | Self::Redact { area: r, .. } => {
                *r = to;
            }
            Self::Freehand(points) => {
                for p in points.iter_mut() {
                    *p = geom::remap(*p, &from, &to);
                }
            }
            Self::Text { at, .. } => *at = to.origin,
            Self::Counter { at, .. } => *at = geom::center(&to),
        }
    }
}

/// A stable, document-scoped identifier for one annotation.
///
/// Ids are assigned in creation order and never reused, which is what lets
/// z-order be reshuffled freely while counter numbering stays stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AnnotationId(pub u64);

/// One annotation plus its identity and appearance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnnotationObject {
    /// Stable identity, unique within its document.
    pub id: AnnotationId,
    /// The geometry.
    pub annotation: Annotation,
    /// How it is drawn.
    pub style: Style,
}

impl AnnotationObject {
    /// Extra slack, in logical points, around an annotation's true outline when
    /// hit-testing.
    ///
    /// A 1-point arrow is impossible to click on its mathematical outline; every
    /// drawing tool grows the target. This is deliberately generous enough for a
    /// trackpad and small enough not to swallow neighbouring objects.
    pub const HIT_TOLERANCE: f64 = 4.0;

    /// Pairs an annotation with an id and a style.
    #[must_use]
    pub const fn new(id: AnnotationId, annotation: Annotation, style: Style) -> Self {
        Self {
            id,
            annotation,
            style,
        }
    }

    /// Which kind of annotation this is.
    #[must_use]
    pub const fn kind(&self) -> AnnotationKind {
        self.annotation.kind()
    }

    /// The annotation's geometric box — the handles a resize gesture drags.
    ///
    /// Deliberately *excludes* stroke and type extent, so that
    /// `set_bounds(object.bounds())` is a no-op. Returning the painted extent
    /// here would make every resize grow the shape by its own stroke width, and
    /// a shape that creeps outwards each time it is nudged is the kind of defect
    /// that is noticed only after it has happened ten times.
    ///
    /// Use [`Self::visual_bounds`] for invalidation and for anything that needs
    /// the pixels actually touched.
    #[must_use]
    pub fn bounds(&self) -> LogicalRect {
        match &self.annotation {
            Annotation::Text { at, content } => LogicalRect::new(
                *at,
                font::measure(content, self.style.effective_font_size()),
            ),
            Annotation::Counter { at, .. } => {
                let r = self.counter_radius();
                geom::from_edges(at.x - r, at.y - r, at.x + r, at.y + r)
            }
            other => other.bounds(),
        }
    }

    /// The box the annotation actually paints into, stroke and arrowhead included.
    #[must_use]
    pub fn visual_bounds(&self) -> LogicalRect {
        let half_stroke = self.style.effective_stroke_width() / 2.0;
        match &self.annotation {
            Annotation::Text { .. } | Annotation::Counter { .. } => self.bounds(),
            Annotation::Arrow { .. } => {
                // The head is wider than the shaft, so grow by its half-width.
                geom::inflate(&self.annotation.bounds(), self.arrow_head_half_width())
            }
            Annotation::Highlight(_) | Annotation::Redact { .. } => self.annotation.bounds(),
            _ => geom::inflate(&self.annotation.bounds(), half_stroke),        }
    }

    /// The radius of a counter marker, derived from its type size.
    #[must_use]
    pub fn counter_radius(&self) -> f64 {
        self.style.effective_font_size() * 0.95
    }

    /// How far an arrowhead reaches back along the shaft.
    ///
    /// Scales with stroke width so a thick arrow does not end in a pinpoint and
    /// a hairline arrow does not end in a blob.
    #[must_use]
    pub fn arrow_head_length(&self) -> f64 {
        self.style.effective_stroke_width() * 3.6
    }

    /// Half the width of an arrowhead at its base.
    #[must_use]
    pub fn arrow_head_half_width(&self) -> f64 {
        self.style.effective_stroke_width() * 1.8
    }

    /// Whether `point` selects this annotation.
    ///
    /// Filled and region-like annotations are hit anywhere inside them; outlined
    /// ones are hit near their outline, so a large empty rectangle does not
    /// block everything it encloses.
    #[must_use]
    pub fn hit(&self, point: LogicalPoint) -> bool {
        if self.style.effective_opacity() <= 0.0 {
            return false;
        }
        let slack = self.style.effective_stroke_width() / 2.0 + Self::HIT_TOLERANCE;
        match &self.annotation {
            Annotation::Arrow { from, to } | Annotation::Line { from, to } => {
                geom::distance_to_segment(point, *from, *to) <= slack
            }
            Annotation::Rectangle(r) => {
                if self.style.fill.is_some_and(|f| !f.is_invisible()) {
                    geom::contains(&geom::inflate(r, slack), point)
                } else {
                    geom::distance_to_rect_outline(point, r) <= slack
                }
            }
            Annotation::Ellipse(r) => {
                if self.style.fill.is_some_and(|f| !f.is_invisible()) {
                    geom::contains_ellipse(&geom::inflate(r, slack), point)
                } else {
                    geom::contains_ellipse(&geom::inflate(r, slack), point)
                        && !geom::contains_ellipse(&geom::inflate(r, -slack), point)
                }
            }
            Annotation::Freehand(points) => {
                points
                    .windows(2)
                    .any(|w| geom::distance_to_segment(point, w[0], w[1]) <= slack)
                    || points
                        .first()
                        .is_some_and(|p| geom::distance(point, *p) <= slack)
            }
            Annotation::Highlight(r) | Annotation::Redact { area: r, .. } => {
                geom::contains(&geom::inflate(r, Self::HIT_TOLERANCE), point)
            }
            Annotation::Text { .. } => {
                geom::contains(&geom::inflate(&self.bounds(), Self::HIT_TOLERANCE), point)
            }
            Annotation::Counter { at, .. } => {
                geom::distance(point, *at) <= self.counter_radius() + Self::HIT_TOLERANCE
            }
        }
    }
}
