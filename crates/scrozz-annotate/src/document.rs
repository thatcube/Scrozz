//! The document: a capture plus every edit ever made to it.

use scrozz_core::{Capture, Error, LogicalPoint, LogicalRect, LogicalSize, Result};
use serde::{Deserialize, Serialize};

use crate::{
    annotation::{Annotation, AnnotationId, AnnotationObject},
    geom,
    style::{Color, Style},
};

/// The background painted behind a beautified capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Background {
    /// Nothing — the padding stays transparent.
    #[default]
    Transparent,
    /// A flat colour.
    Solid(Color),
    /// A vertical gradient from `start` at the top to `end` at the bottom.
    Gradient {
        /// Colour at the top edge.
        start: Color,
        /// Colour at the bottom edge.
        end: Color,
    },
}

/// Padding, background and framing applied around a capture.
///
/// Per decision D9 this is refused outright for window captures: the OS already
/// supplied the window's true shape and shadow, and synthesising them again
/// yields a subtly, unmistakably wrong image. [`Document::may_beautify`] is the
/// enforcement point, [`Document::set_beautification`] is the gate, and the
/// renderer refuses a second time so a document assembled by any other route
/// still cannot slip through.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Beautification {
    /// Padding around the image, in logical points.
    pub padding: f64,
    /// Corner radius applied to the image.
    pub corner_radius: f64,
    /// Drop shadow depth.
    pub shadow: f64,
    /// What fills the padding.
    #[serde(default)]
    pub background: Background,
}

impl Beautification {
    /// A preset: generous padding on a flat neutral background, no shadow.
    #[must_use]
    pub fn padded(padding: f64, background: Background) -> Self {
        Self {
            padding,
            corner_radius: 0.0,
            shadow: 0.0,
            background,
        }
    }

    /// The radius a shape nested inside another must use to look concentric.
    ///
    /// D9's corollary: `inner_radius = outer_radius − padding`. Nesting two
    /// rounded shapes at the *same* radius is the specific mistake that makes
    /// corners look subtly wrong even though both shapes are "rounded".
    #[must_use]
    pub fn nested_radius(outer_radius: f64, padding: f64) -> f64 {
        (outer_radius - padding).max(0.0)
    }

    /// Whether this would visibly change the image at all.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.padding <= 0.0
            && self.corner_radius <= 0.0
            && self.shadow <= 0.0
            && self.background == Background::Transparent
    }
}

/// The editable part of a document: everything except the pixels.
///
/// Per decision D14 this is persisted invisibly alongside the capture rather
/// than exposed as a `.scrozz` project file, so reopening a capture months later
/// restores every arrow with nothing for the user to have managed or lost. It is
/// deliberately an internal, unadvertised format: keeping it unpublished is what
/// lets it change freely while the tool set is still being designed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentData {
    /// Format version, so an old document can be migrated rather than rejected.
    pub version: u32,
    /// Edits, in z-order: last is on top.
    pub annotations: Vec<AnnotationObject>,
    /// Framing, if permitted.
    pub beautification: Option<Beautification>,
    /// The visible region of the source, if it has been cropped.
    ///
    /// In source-logical coordinates, and never applied to the pixels: the
    /// source keeps every pixel it was captured with, so a crop can be widened
    /// again — or cleared entirely — months later. Annotations outside it are
    /// kept too, and simply fall outside the rendered area.
    #[serde(default)]
    pub crop: Option<LogicalRect>,
    /// The next identifier to hand out.
    ///
    /// Persisted so a reopened document cannot reissue an id that an undo stack
    /// or a selection still refers to.
    pub next_id: u64,
}

impl DocumentData {
    /// The current format version.
    pub const VERSION: u32 = 1;
}

impl Default for DocumentData {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            annotations: Vec::new(),
            beautification: None,
            crop: None,
            next_id: 1,
        }
    }
}

/// A capture plus every edit ever made to it.
///
/// The annotation list is private on purpose. Two invariants have to hold for
/// the document to behave the way decision D14 promises — identifiers are unique
/// and never reused, and counter markers stay numbered 1..n with no gaps — and
/// neither survives a `pub Vec` that any caller can splice.
#[derive(Debug, Clone)]
pub struct Document {
    /// The untouched source. Never mutated.
    ///
    /// Rendering copies before it composites, and redaction destroys pixels only
    /// in that copy. A redacted export is unrecoverable; the document it came
    /// from is still fully editable.
    pub source: Capture,
    objects: Vec<AnnotationObject>,
    beautification: Option<Beautification>,
    crop: Option<LogicalRect>,
    next_id: u64,
}

impl Document {
    /// Wraps a fresh capture in an empty document.
    #[must_use]
    pub fn new(source: Capture) -> Self {
        Self {
            source,
            objects: Vec::new(),
            beautification: None,
            crop: None,
            next_id: 1,
        }
    }

    /// Rebuilds a document from a capture and its persisted edits.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the data is from a newer format
    /// version, or if it carries beautification for a capture that forbids it —
    /// a document that was hand-edited or that changed provenance must not be
    /// silently accepted and then quietly rendered wrong.
    pub fn from_data(source: Capture, data: DocumentData) -> Result<Self> {
        if data.version > DocumentData::VERSION {
            return Err(Error::InvalidRequest(format!(
                "document format version {} is newer than supported version {}",
                data.version,
                DocumentData::VERSION
            )));
        }
        if data.beautification.is_some() && source.provenance.forbids_compositing() {
            return Err(Error::InvalidRequest(
                "beautification is not permitted for window captures (decision D9)".to_owned(),
            ));
        }
        let highest = data
            .annotations
            .iter()
            .map(|o| o.id.0)
            .max()
            .map_or(0, |id| id + 1);
        let mut document = Self {
            source,
            objects: data.annotations,
            beautification: data.beautification,
            crop: None,
            next_id: data.next_id.max(highest).max(1),
        };
        document.set_crop(data.crop)?;
        document.renumber_counters();
        Ok(document)
    }

    /// The editable part of this document, ready to persist.
    #[must_use]
    pub fn data(&self) -> DocumentData {
        DocumentData {
            version: DocumentData::VERSION,
            annotations: self.objects.clone(),
            beautification: self.beautification.clone(),
            crop: self.crop,
            next_id: self.next_id,
        }
    }

    /// Replaces every editable part of this document at once.
    ///
    /// The source is untouched — a snapshot only ever travels between states of
    /// the same document, so restoring one must not be able to swap the image
    /// out from under it.
    ///
    /// # Errors
    ///
    /// The same conditions as [`Self::from_data`].
    pub fn restore(&mut self, data: DocumentData) -> Result<()> {
        let restored = Self::from_data(self.source.clone(), data)?;
        self.objects = restored.objects;
        self.beautification = restored.beautification;
        self.crop = restored.crop;
        self.next_id = restored.next_id;
        Ok(())
    }

    /// Every annotation, bottom-most first.
    #[must_use]
    pub fn annotations(&self) -> &[AnnotationObject] {
        &self.objects
    }

    /// How many annotations the document holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Whether the document has no annotations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// The source image's size in logical points.
    ///
    /// This, not the pixel size, is the space annotations are authored in.
    #[must_use]
    pub fn logical_size(&self) -> LogicalSize {
        let scale = self.source.frame.scale.get();
        LogicalSize::new(
            self.source.frame.size.width / scale,
            self.source.frame.size.height / scale,
        )
    }

    /// The whole source image as a logical rectangle.
    #[must_use]
    pub fn logical_bounds(&self) -> LogicalRect {
        LogicalRect::new(LogicalPoint::new(0.0, 0.0), self.logical_size())
    }

    /// The crop, if the document has been cropped.
    #[must_use]
    pub fn crop(&self) -> Option<LogicalRect> {
        self.crop
    }

    /// The region that renders: the crop if there is one, else the whole image.
    #[must_use]
    pub fn content_bounds(&self) -> LogicalRect {
        self.crop.unwrap_or_else(|| self.logical_bounds())
    }

    /// The rendered size in logical points.
    #[must_use]
    pub fn content_size(&self) -> LogicalSize {
        self.content_bounds().size
    }

    /// Crops the document to `area`, or clears the crop with `None`.
    ///
    /// The rectangle is clamped to the source: a crop dragged past the edge
    /// trims to the edge rather than inventing transparent margin, which is
    /// what the drag gesture visibly promises.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the rectangle is not finite, or if
    /// clamping leaves it with no area — an empty crop would render a
    /// zero-pixel image, and silently ignoring the request would leave the
    /// editor showing a selection the document does not have.
    pub fn set_crop(&mut self, area: Option<LogicalRect>) -> Result<()> {
        let Some(area) = area else {
            self.crop = None;
            return Ok(());
        };
        if ![
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
        ]
        .iter()
        .all(|v| v.is_finite())
        {
            return Err(Error::InvalidRequest(
                "crop rectangle must be finite".to_owned(),
            ));
        }
        let bounds = self.logical_bounds();
        let left = area.origin.x.max(bounds.origin.x);
        let top = area.origin.y.max(bounds.origin.y);
        let right = geom::max_x(&area).min(geom::max_x(&bounds));
        let bottom = geom::max_y(&area).min(geom::max_y(&bounds));
        if right - left <= 0.0 || bottom - top <= 0.0 {
            return Err(Error::InvalidRequest(
                "crop rectangle does not overlap the capture".to_owned(),
            ));
        }
        let clamped = geom::from_edges(left, top, right, bottom);
        // A crop that covers everything is no crop: storing it would make
        // `crop()` report a crop the user cannot see and cannot clear.
        self.crop = (clamped != bounds).then_some(clamped);
        Ok(())
    }

    /// Adds an annotation on top of everything else.
    ///
    /// Counter markers are numbered by the document, so the `index` on a
    /// [`Annotation::Counter`] passed in here is ignored and replaced.
    pub fn add(&mut self, annotation: Annotation, style: Style) -> AnnotationId {
        let id = AnnotationId(self.next_id);
        self.next_id += 1;
        self.objects
            .push(AnnotationObject::new(id, annotation, style));
        self.renumber_counters();
        id
    }

    /// Adds an annotation with the default style for its kind.
    pub fn add_default(&mut self, annotation: Annotation) -> AnnotationId {
        let style = match &annotation {
            Annotation::Highlight(_) => Style::highlighter(),
            Annotation::Redact { .. } => Style::redaction(),
            _ => Style::stroked(),
        };
        self.add(annotation, style)
    }

    /// Removes an annotation, renumbering counters to close the gap.
    pub fn remove(&mut self, id: AnnotationId) -> Option<AnnotationObject> {
        let index = self.index_of(id)?;
        let removed = self.objects.remove(index);
        self.renumber_counters();
        Some(removed)
    }

    /// Removes every annotation, leaving the source untouched.
    pub fn clear(&mut self) {
        self.objects.clear();
    }

    /// Looks up an annotation.
    #[must_use]
    pub fn get(&self, id: AnnotationId) -> Option<&AnnotationObject> {
        self.objects.iter().find(|o| o.id == id)
    }

    /// Looks up an annotation for editing.
    ///
    /// Counter numbering is re-derived after any edit made through this handle,
    /// so a caller cannot leave the sequence inconsistent.
    pub fn get_mut(&mut self, id: AnnotationId) -> Option<AnnotationMut<'_>> {
        let index = self.index_of(id)?;
        Some(AnnotationMut {
            document: self,
            index,
        })
    }

    /// Replaces one annotation's style.
    pub fn set_style(&mut self, id: AnnotationId, style: Style) -> bool {
        match self.index_of(id) {
            Some(index) => {
                self.objects[index].style = style;
                true
            }
            None => false,
        }
    }

    /// Moves an annotation by `dx`, `dy` logical points.
    pub fn translate(&mut self, id: AnnotationId, dx: f64, dy: f64) -> bool {
        match self.index_of(id) {
            Some(index) => {
                self.objects[index].annotation.translate(dx, dy);
                true
            }
            None => false,
        }
    }

    /// Reshapes an annotation to fill `bounds`.
    pub fn set_bounds(&mut self, id: AnnotationId, bounds: LogicalRect) -> bool {
        match self.index_of(id) {
            Some(index) => {
                self.objects[index].annotation.set_bounds(bounds);
                true
            }
            None => false,
        }
    }

    /// The top-most annotation under `point`, if any.
    ///
    /// Top-most is what a click means: the object the user can see at that
    /// position is the one they are pointing at.
    #[must_use]
    pub fn hit_test(&self, point: LogicalPoint) -> Option<AnnotationId> {
        self.objects
            .iter()
            .rev()
            .find(|o| o.hit(point))
            .map(|o| o.id)
    }

    /// Every annotation under `point`, top-most first.
    #[must_use]
    pub fn hit_test_all(&self, point: LogicalPoint) -> Vec<AnnotationId> {
        self.objects
            .iter()
            .rev()
            .filter(|o| o.hit(point))
            .map(|o| o.id)
            .collect()
    }

    /// Moves an annotation above every other.
    pub fn bring_to_front(&mut self, id: AnnotationId) -> bool {
        match self.index_of(id) {
            Some(index) => {
                let object = self.objects.remove(index);
                self.objects.push(object);
                true
            }
            None => false,
        }
    }

    /// Moves an annotation below every other.
    pub fn send_to_back(&mut self, id: AnnotationId) -> bool {
        match self.index_of(id) {
            Some(index) => {
                let object = self.objects.remove(index);
                self.objects.insert(0, object);
                true
            }
            None => false,
        }
    }

    /// Moves an annotation one step up in z-order.
    pub fn raise(&mut self, id: AnnotationId) -> bool {
        match self.index_of(id) {
            Some(index) if index + 1 < self.objects.len() => {
                self.objects.swap(index, index + 1);
                true
            }
            _ => false,
        }
    }

    /// Moves an annotation one step down in z-order.
    pub fn lower(&mut self, id: AnnotationId) -> bool {
        match self.index_of(id) {
            Some(index) if index > 0 => {
                self.objects.swap(index, index - 1);
                true
            }
            _ => false,
        }
    }

    /// The annotation's position in z-order, if it exists.
    #[must_use]
    pub fn z_index(&self, id: AnnotationId) -> Option<usize> {
        self.index_of(id)
    }

    /// Whether framing may be applied at all.
    ///
    /// False for window captures. The UI disables the controls entirely rather
    /// than letting them be set and quietly ignored.
    #[must_use]
    pub fn may_beautify(&self) -> bool {
        !self.source.provenance.forbids_compositing()
    }

    /// The framing currently applied, if any.
    #[must_use]
    pub fn beautification(&self) -> Option<&Beautification> {
        self.beautification.as_ref()
    }

    /// Applies or clears framing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when framing is requested for a window
    /// capture. Per decision D9 the OS output *is* the truth for a window, and
    /// compositing padding, corners or a shadow onto it produces a subtly wrong
    /// image. Refusing here, rather than accepting and ignoring, is what stops a
    /// caller believing the setting took effect.
    pub fn set_beautification(&mut self, beautification: Option<Beautification>) -> Result<()> {
        if beautification.is_some() && !self.may_beautify() {
            return Err(Error::InvalidRequest(
                "beautification is not permitted for window captures (decision D9)".to_owned(),
            ));
        }
        self.beautification = beautification;
        Ok(())
    }

    /// The highest number currently assigned to a counter marker.
    #[must_use]
    pub fn counter_count(&self) -> u32 {
        self.objects
            .iter()
            .filter(|o| matches!(o.annotation, Annotation::Counter { .. }))
            .count() as u32
    }

    fn index_of(&self, id: AnnotationId) -> Option<usize> {
        self.objects.iter().position(|o| o.id == id)
    }

    /// Renumbers counter markers 1..n in creation order.
    ///
    /// Creation order, not z-order: raising a marker to the front must not
    /// silently renumber the whole sequence, and identifiers are handed out
    /// monotonically, so sorting by id recovers the order they were drawn in.
    fn renumber_counters(&mut self) {
        let mut counters: Vec<(AnnotationId, usize)> = self
            .objects
            .iter()
            .enumerate()
            .filter(|(_, o)| matches!(o.annotation, Annotation::Counter { .. }))
            .map(|(i, o)| (o.id, i))
            .collect();
        counters.sort_unstable_by_key(|(id, _)| *id);
        for (number, (_, index)) in counters.into_iter().enumerate() {
            if let Annotation::Counter { index: n, .. } = &mut self.objects[index].annotation {
                *n = number as u32 + 1;
            }
        }
    }
}

/// A borrowed, invariant-preserving handle to one annotation.
///
/// Dropping it re-derives counter numbering, so an edit that turns something
/// into (or away from) a counter cannot leave a gap in the sequence.
#[derive(Debug)]
pub struct AnnotationMut<'a> {
    document: &'a mut Document,
    index: usize,
}

impl AnnotationMut<'_> {
    /// The annotation being edited.
    #[must_use]
    pub fn object(&mut self) -> &mut AnnotationObject {
        &mut self.document.objects[self.index]
    }

    /// The geometry being edited.
    #[must_use]
    pub fn annotation(&mut self) -> &mut Annotation {
        &mut self.document.objects[self.index].annotation
    }

    /// The style being edited.
    #[must_use]
    pub fn style(&mut self) -> &mut Style {
        &mut self.document.objects[self.index].style
    }
}

impl Drop for AnnotationMut<'_> {
    fn drop(&mut self) {
        self.document.renumber_counters();
    }
}
