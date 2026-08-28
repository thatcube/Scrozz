//! The annotation editor's state machine.
//!
//! Pure and headless on purpose. Everything here works in *document logical
//! points* — the same space annotations are authored in — and knows nothing
//! about `egui`, textures, or where on screen the canvas happens to be. The
//! painter converts screen positions into this space once, on the way in, and
//! back out again on the way to the screen.
//!
//! That split is what makes the editor testable. A drag that creates an arrow,
//! a click that selects the object underneath, a handle drag that resizes it,
//! and the keyboard accelerators that delete it are all exercised with no
//! window, no display and no golden image.
//!
//! # The document is the preview
//!
//! Gestures mutate the [`Document`] live rather than accumulating a pending
//! edit applied on release. The preview is a render of the document, so
//! anything not committed to the document would not appear in it — and a
//! preview that diverges from the export is the specific failure this design
//! exists to prevent. Undo depth is protected instead by `History::commit`,
//! which ignores a commit recording no change, so a whole drag still costs
//! exactly one step.

use scrozz_annotate::{
    Annotation, AnnotationId, AnnotationKind, Color, Document, History, RedactStyle, Style, geom,
};
use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize, Result};

/// How close, in logical points, a pointer must come to a resize handle.
pub const HANDLE_TOLERANCE: f64 = 7.0;

/// The visual radius of a resize handle, in logical points.
pub const HANDLE_RADIUS: f64 = 4.5;

/// The shortest drag, in logical points, that creates a shape.
///
/// Below this a press-drag-release is treated as a click: a stray one-pixel
/// twitch while clicking must not leave an invisible zero-size rectangle in the
/// document for the user to wonder about later.
pub const MIN_DRAG: f64 = 3.0;

/// The smallest a shape may be resized to, in logical points.
pub const MIN_SIZE: f64 = 4.0;

/// How far one arrow-key nudge moves a selection, in logical points.
pub const NUDGE: f64 = 1.0;

/// How far a shift-arrow nudge moves a selection, in logical points.
pub const NUDGE_COARSE: f64 = 10.0;

/// The smallest zoom the editor allows.
pub const MIN_ZOOM: f32 = 0.1;

/// The largest zoom the editor allows.
pub const MAX_ZOOM: f32 = 8.0;

/// The multiplier applied by one zoom-in or zoom-out step.
pub const ZOOM_STEP: f32 = 1.25;

/// The thinnest stroke the editor offers.
///
/// Lives here rather than on the toolbar because the state enforces it:
/// accelerators and scripted edits reach [`EditorState::set_stroke_width`]
/// without passing a slider, and a stroke of zero renders as nothing.
pub const STROKE_MIN: f64 = 1.0;

/// The thickest stroke the editor offers.
pub const STROKE_MAX: f64 = 24.0;

/// Which tool the pointer is currently holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Tool {
    /// Select, move and resize existing annotations.
    #[default]
    Select,
    /// Draw an arrow.
    Arrow,
    /// Draw a plain line.
    Line,
    /// Draw a rectangle.
    Rectangle,
    /// Draw an ellipse.
    Ellipse,
    /// Draw freehand.
    Pen,
    /// Place a text label.
    Text,
    /// Draw a highlighter band.
    Highlight,
    /// Draw a blur redaction.
    Blur,
    /// Draw a pixelate redaction.
    Pixelate,
    /// Place the next numbered step marker.
    Counter,
    /// Drag out a crop.
    Crop,
}

impl Tool {
    /// Every tool, in palette order.
    pub const ALL: [Self; 12] = [
        Self::Select,
        Self::Arrow,
        Self::Line,
        Self::Rectangle,
        Self::Ellipse,
        Self::Pen,
        Self::Text,
        Self::Highlight,
        Self::Blur,
        Self::Pixelate,
        Self::Counter,
        Self::Crop,
    ];

    /// The tool's name, for tooltips and accessibility.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Select => "Select",
            Self::Arrow => "Arrow",
            Self::Line => "Line",
            Self::Rectangle => "Rectangle",
            Self::Ellipse => "Ellipse",
            Self::Pen => "Pen",
            Self::Text => "Text",
            Self::Highlight => "Highlight",
            Self::Blur => "Blur",
            Self::Pixelate => "Pixelate",
            Self::Counter => "Step number",
            Self::Crop => "Crop",
        }
    }

    /// The single-key accelerator that picks this tool.
    ///
    /// Chosen to match what the tool is called rather than its palette order,
    /// because a user reaches for `r` meaning "rectangle", not "the fourth one".
    #[must_use]
    pub const fn accelerator(self) -> char {
        match self {
            Self::Select => 'v',
            Self::Arrow => 'a',
            Self::Line => 'l',
            Self::Rectangle => 'r',
            Self::Ellipse => 'o',
            Self::Pen => 'p',
            Self::Text => 't',
            Self::Highlight => 'h',
            Self::Blur => 'b',
            Self::Pixelate => 'x',
            Self::Counter => 'n',
            Self::Crop => 'c',
        }
    }

    /// The tool an accelerator picks, if any.
    #[must_use]
    pub fn from_accelerator(key: char) -> Option<Self> {
        let key = key.to_ascii_lowercase();
        Self::ALL.into_iter().find(|t| t.accelerator() == key)
    }

    /// Whether this tool draws by dragging out a rectangle.
    #[must_use]
    pub const fn is_rect_drag(self) -> bool {
        matches!(
            self,
            Self::Rectangle | Self::Ellipse | Self::Highlight | Self::Blur | Self::Pixelate
        )
    }

    /// Whether this tool places its annotation with a single click.
    #[must_use]
    pub const fn is_click_place(self) -> bool {
        matches!(self, Self::Text | Self::Counter)
    }

    /// The annotation kind this tool produces, if it produces one.
    ///
    /// [`Tool::Select`] and [`Tool::Crop`] have none: they act on the document
    /// rather than adding to it.
    #[must_use]
    pub const fn kind(self) -> Option<AnnotationKind> {
        Some(match self {
            Self::Select | Self::Crop => return None,
            Self::Arrow => AnnotationKind::Arrow,
            Self::Line => AnnotationKind::Line,
            Self::Rectangle => AnnotationKind::Rectangle,
            Self::Ellipse => AnnotationKind::Ellipse,
            Self::Pen => AnnotationKind::Freehand,
            Self::Text => AnnotationKind::Text,
            Self::Highlight => AnnotationKind::Highlight,
            Self::Blur | Self::Pixelate => AnnotationKind::Redact,
            Self::Counter => AnnotationKind::Counter,
        })
    }
}

/// Which corner or edge of a selection a drag is pulling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Handle {
    /// Top-left corner.
    TopLeft,
    /// Top edge.
    Top,
    /// Top-right corner.
    TopRight,
    /// Right edge.
    Right,
    /// Bottom-right corner.
    BottomRight,
    /// Bottom edge.
    Bottom,
    /// Bottom-left corner.
    BottomLeft,
    /// Left edge.
    Left,
}

impl Handle {
    /// Every handle, clockwise from the top-left.
    pub const ALL: [Self; 8] = [
        Self::TopLeft,
        Self::Top,
        Self::TopRight,
        Self::Right,
        Self::BottomRight,
        Self::Bottom,
        Self::BottomLeft,
        Self::Left,
    ];

    /// Where this handle sits on a rectangle.
    #[must_use]
    pub fn position(self, rect: &LogicalRect) -> LogicalPoint {
        let (l, t) = (rect.origin.x, rect.origin.y);
        let (r, b) = (geom::max_x(rect), geom::max_y(rect));
        let (cx, cy) = ((l + r) / 2.0, (t + b) / 2.0);
        match self {
            Self::TopLeft => LogicalPoint::new(l, t),
            Self::Top => LogicalPoint::new(cx, t),
            Self::TopRight => LogicalPoint::new(r, t),
            Self::Right => LogicalPoint::new(r, cy),
            Self::BottomRight => LogicalPoint::new(r, b),
            Self::Bottom => LogicalPoint::new(cx, b),
            Self::BottomLeft => LogicalPoint::new(l, b),
            Self::Left => LogicalPoint::new(l, cy),
        }
    }

    /// Whether this handle moves the left edge.
    #[must_use]
    pub const fn moves_left(self) -> bool {
        matches!(self, Self::TopLeft | Self::Left | Self::BottomLeft)
    }

    /// Whether this handle moves the right edge.
    #[must_use]
    pub const fn moves_right(self) -> bool {
        matches!(self, Self::TopRight | Self::Right | Self::BottomRight)
    }

    /// Whether this handle moves the top edge.
    #[must_use]
    pub const fn moves_top(self) -> bool {
        matches!(self, Self::TopLeft | Self::Top | Self::TopRight)
    }

    /// Whether this handle moves the bottom edge.
    #[must_use]
    pub const fn moves_bottom(self) -> bool {
        matches!(self, Self::BottomLeft | Self::Bottom | Self::BottomRight)
    }

    /// Applies a drag of this handle to a rectangle.
    ///
    /// Edges are sorted afterwards, so dragging a corner past its opposite
    /// flips the rectangle rather than inverting it — which is what the gesture
    /// looks like it is doing, and what every other editor does.
    #[must_use]
    pub fn resize(self, rect: &LogicalRect, dx: f64, dy: f64) -> LogicalRect {
        let mut l = rect.origin.x;
        let mut t = rect.origin.y;
        let mut r = geom::max_x(rect);
        let mut b = geom::max_y(rect);
        if self.moves_left() {
            l += dx;
        }
        if self.moves_right() {
            r += dx;
        }
        if self.moves_top() {
            t += dy;
        }
        if self.moves_bottom() {
            b += dy;
        }
        geom::from_edges(l.min(r), t.min(b), l.max(r), t.max(b))
    }
}

/// What the pointer is doing between press and release.
#[derive(Debug, Clone, PartialEq)]
enum Drag {
    Idle,
    Creating {
        origin: LogicalPoint,
        id: Option<AnnotationId>,
    },
    Drawing {
        id: AnnotationId,
        points: Vec<LogicalPoint>,
    },
    Moving {
        grab: LogicalPoint,
        start: LogicalRect,
    },
    Resizing {
        handle: Handle,
        grab: LogicalPoint,
        start: LogicalRect,
    },
    Cropping {
        origin: LogicalPoint,
        current: LogicalPoint,
    },
    Panning {
        grab: (f32, f32),
        start: (f32, f32),
    },
}

/// A keyboard command the editor understands.
///
/// The state machine maps keys to these rather than acting on key codes
/// directly, so the same accelerator table can be shown in a menu, asserted in
/// a test, and driven from a scene fixture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    /// Cancel the gesture, clear the selection, or close the editor.
    Escape,
    /// Delete the selection.
    Delete,
    /// Undo one step.
    Undo,
    /// Redo one step.
    Redo,
    /// Copy the flattened image to the clipboard.
    Copy,
    /// Save the flattened image.
    Save,
    /// Select the topmost annotation.
    SelectAll,
    /// Pick a tool.
    Pick(Tool),
    /// Nudge the selection.
    Nudge {
        /// Horizontal points.
        dx: f64,
        /// Vertical points.
        dy: f64,
    },
    /// Zoom in one step.
    ZoomIn,
    /// Zoom out one step.
    ZoomOut,
    /// Reset zoom and pan.
    ZoomReset,
    /// Commit the crop currently being dragged.
    ApplyCrop,
    /// Send the selection behind everything else.
    SendToBack,
    /// Bring the selection in front of everything else.
    BringToFront,
}

/// What the editor wants its host to do, after handling a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Intent {
    /// Nothing; stay open.
    #[default]
    None,
    /// Close the editor.
    Close,
    /// Copy the flattened render to the clipboard.
    Copy,
    /// Save the flattened render.
    Save,
}

/// The editor's whole mutable state.
///
/// Owns the document and its history together, because every edit has to touch
/// both and separating them invites a mutation that never gets committed.
#[derive(Debug)]
pub struct EditorState {
    document: Document,
    history: History,
    tool: Tool,
    style: Style,
    selection: Option<AnnotationId>,
    drag: Drag,
    editing_text: Option<AnnotationId>,
    zoom: f32,
    pan: (f32, f32),
    revision: u64,
    dirty: bool,
}

impl EditorState {
    /// Opens an editor over `document`.
    #[must_use]
    pub fn new(document: Document) -> Self {
        let history = History::new(&document);
        Self {
            document,
            history,
            tool: Tool::Select,
            style: Style::stroked().with_stroke(Color::ACCENT),
            selection: None,
            drag: Drag::Idle,
            editing_text: None,
            zoom: 1.0,
            pan: (0.0, 0.0),
            revision: 0,
            dirty: false,
        }
    }

    /// The document being edited.
    #[must_use]
    pub const fn document(&self) -> &Document {
        &self.document
    }

    /// A counter that changes whenever what is drawn would change.
    ///
    /// The painter caches its preview texture against this rather than
    /// re-rendering every frame: a full composite of a 5K capture is far too
    /// slow for 60 fps, and re-doing it when nothing moved would make the
    /// editor unusable on exactly the large captures it matters for.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Whether anything has been edited since the editor opened.
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The tool in hand.
    #[must_use]
    pub const fn tool(&self) -> Tool {
        self.tool
    }

    /// Picks a tool, cancelling any gesture in progress.
    pub fn set_tool(&mut self, tool: Tool) {
        if self.tool == tool {
            return;
        }
        self.cancel_drag();
        self.finish_text();
        self.tool = tool;
        // Select is the only tool where a selection is meaningful; keeping one
        // while a drawing tool is active would show handles the user cannot use.
        if tool != Tool::Select {
            self.selection = None;
        }
        self.touch();
    }

    /// The style new annotations are drawn with.
    #[must_use]
    pub const fn style(&self) -> &Style {
        &self.style
    }

    /// Sets the drawing style, and restyles the selection if there is one.
    ///
    /// Applying to the selection is what makes the palette feel direct: picking
    /// red with an arrow selected should turn *that* arrow red, not merely arm
    /// the next one.
    pub fn set_style(&mut self, style: Style) {
        self.style = style;
        if let Some(id) = self.selection {
            let restyled = restyle_for(&self.document, id, style);
            self.document.set_style(id, restyled);
            self.commit_coalesced("style");
        }
        self.touch();
    }

    /// The stroke colour in hand.
    #[must_use]
    pub const fn stroke_color(&self) -> Color {
        self.style.stroke
    }

    /// Sets the stroke colour.
    pub fn set_stroke_color(&mut self, color: Color) {
        let style = self.style.with_stroke(color);
        self.set_style(style);
    }

    /// The stroke width in hand.
    #[must_use]
    pub fn stroke_width(&self) -> f64 {
        self.style.effective_stroke_width()
    }

    /// Sets the stroke width, clamped to what the editor can draw.
    ///
    /// Clamping here rather than only in the slider keeps a keyboard or
    /// scripted change from producing an invisible hairline or a stroke wider
    /// than the shape it outlines.
    pub fn set_stroke_width(&mut self, width: f64) {
        let style = self
            .style
            .with_stroke_width(width.clamp(STROKE_MIN, STROKE_MAX));
        self.set_style(style);
    }

    /// The selected annotation, if any.
    #[must_use]
    pub const fn selection(&self) -> Option<AnnotationId> {
        self.selection
    }

    /// The selection's bounding box, if there is a selection.
    #[must_use]
    pub fn selection_bounds(&self) -> Option<LogicalRect> {
        self.selection
            .and_then(|id| self.document.get(id))
            .map(scrozz_annotate::AnnotationObject::bounds)
    }

    /// Selects an annotation, or clears the selection with `None`.
    pub fn select(&mut self, id: Option<AnnotationId>) {
        if self.selection == id {
            return;
        }
        self.selection = id.filter(|id| self.document.get(*id).is_some());
        self.touch();
    }

    /// The annotation whose text is being typed, if any.
    #[must_use]
    pub const fn editing_text(&self) -> Option<AnnotationId> {
        self.editing_text
    }

    /// The text of the annotation being edited.
    #[must_use]
    pub fn text_buffer(&self) -> Option<&str> {
        let id = self.editing_text?;
        match &self.document.get(id)?.annotation {
            Annotation::Text { content, .. } => Some(content.as_str()),
            _ => None,
        }
    }

    /// Replaces the text of the annotation being edited.
    pub fn set_text_buffer(&mut self, text: &str) {
        let Some(id) = self.editing_text else {
            return;
        };
        let Some(mut object) = self.document.get_mut(id) else {
            return;
        };
        let changed = match object.annotation() {
            Annotation::Text { content, .. } if content != text => {
                content.clear();
                content.push_str(text);
                true
            }
            _ => false,
        };
        drop(object);
        if changed {
            self.commit_coalesced("text");
            self.touch();
        }
    }

    /// Finishes text entry, removing the annotation if nothing was typed.
    ///
    /// An empty text annotation is invisible but selectable, which reads as a
    /// bug: clicking to place text and then thinking better of it must leave
    /// nothing behind.
    pub fn finish_text(&mut self) {
        let Some(id) = self.editing_text.take() else {
            return;
        };
        let empty = matches!(
            self.document.get(id).map(|o| &o.annotation),
            Some(Annotation::Text { content, .. }) if content.trim().is_empty()
        );
        if empty {
            self.document.remove(id);
            if self.selection == Some(id) {
                self.selection = None;
            }
        }
        self.history.seal();
        self.commit();
        self.touch();
    }

    /// The current zoom factor.
    #[must_use]
    pub const fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Sets the zoom, clamped to the supported range.
    pub fn set_zoom(&mut self, zoom: f32) {
        let clamped = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if (clamped - self.zoom).abs() > f32::EPSILON {
            self.zoom = clamped;
            self.touch();
        }
    }

    /// The pan offset, in screen points.
    #[must_use]
    pub const fn pan(&self) -> (f32, f32) {
        self.pan
    }

    /// Sets the pan offset.
    pub fn set_pan(&mut self, pan: (f32, f32)) {
        self.pan = pan;
        self.touch();
    }

    /// Whether undo is available.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    /// Whether redo is available.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// The crop being dragged out, if the crop tool is mid-gesture.
    #[must_use]
    pub fn pending_crop(&self) -> Option<LogicalRect> {
        match &self.drag {
            Drag::Cropping { origin, current } => {
                let rect = LogicalRect::from_corners(*origin, *current);
                (rect.size.width >= MIN_SIZE && rect.size.height >= MIN_SIZE).then_some(rect)
            }
            _ => None,
        }
    }

    /// Whether a pointer gesture is in progress.
    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.drag != Drag::Idle
    }

    // -----------------------------------------------------------------------
    // Pointer
    // -----------------------------------------------------------------------

    /// Handles a primary-button press at `point`, in document coordinates.
    pub fn pointer_pressed(&mut self, point: LogicalPoint) {
        if self.editing_text.is_some() {
            self.finish_text();
        }
        match self.tool {
            Tool::Select => self.press_select(point),
            Tool::Crop => {
                self.drag = Drag::Cropping {
                    origin: point,
                    current: point,
                };
            }
            Tool::Pen => {
                let id = self
                    .document
                    .add(Annotation::Freehand(vec![point]), self.style);
                self.selection = None;
                self.drag = Drag::Drawing {
                    id,
                    points: vec![point],
                };
            }
            tool if tool.is_click_place() => self.place(tool, point),
            _ => {
                self.drag = Drag::Creating {
                    origin: point,
                    id: None,
                };
            }
        }
        self.touch();
    }

    /// Handles the pointer moving to `point` while the button is down.
    ///
    /// `constrain` is the shift key: it snaps free endpoints to an axis or the
    /// diagonal, and locks a move to one axis.
    pub fn pointer_dragged(&mut self, point: LogicalPoint, constrain: bool) {
        match std::mem::replace(&mut self.drag, Drag::Idle) {
            Drag::Idle => {}
            Drag::Creating { origin, id } => {
                // A rectangle constrains to a square; a two-point shape
                // constrains to an axis or the diagonal. Squaring an arrow
                // would be meaningless and flattening a rectangle collapses it
                // to a zero-height sliver, so the two cannot share one rule.
                let end = if constrain {
                    if self.tool.is_rect_drag() {
                        square_from(origin, point)
                    } else {
                        constrain_to_axis(origin, point)
                    }
                } else {
                    point
                };
                let id = self.grow(origin, end, id);
                self.drag = Drag::Creating { origin, id };
            }
            Drag::Drawing { id, mut points } => {
                // Drop samples landing on the previous one: a stationary
                // pointer would otherwise pile up thousands of duplicates, all
                // of which get persisted and re-rendered.
                if points
                    .last()
                    .is_none_or(|last| geom::distance(*last, point) > 0.5)
                {
                    points.push(point);
                    if let Some(mut object) = self.document.get_mut(id) {
                        *object.annotation() = Annotation::Freehand(points.clone());
                    }
                }
                self.drag = Drag::Drawing { id, points };
            }
            Drag::Moving { grab, start } => {
                let (dx, dy) = if constrain {
                    axis_lock(point.x - grab.x, point.y - grab.y)
                } else {
                    (point.x - grab.x, point.y - grab.y)
                };
                if let Some(id) = self.selection {
                    let moved = LogicalRect::new(
                        LogicalPoint::new(start.origin.x + dx, start.origin.y + dy),
                        start.size,
                    );
                    self.document.set_bounds(id, moved);
                }
                self.drag = Drag::Moving { grab, start };
            }
            Drag::Resizing {
                handle,
                grab,
                start,
            } => {
                let resized = handle.resize(&start, point.x - grab.x, point.y - grab.y);
                if let Some(id) = self.selection {
                    self.document.set_bounds(id, enforce_min(resized));
                }
                self.drag = Drag::Resizing {
                    handle,
                    grab,
                    start,
                };
            }
            Drag::Cropping { origin, .. } => {
                self.drag = Drag::Cropping {
                    origin,
                    current: if constrain {
                        square_from(origin, point)
                    } else {
                        point
                    },
                };
            }
            Drag::Panning { grab, start } => self.drag = Drag::Panning { grab, start },
        }
        self.touch();
    }

    /// Handles the primary button being released.
    pub fn pointer_released(&mut self) {
        let drag = std::mem::replace(&mut self.drag, Drag::Idle);
        match drag {
            Drag::Creating { id: Some(id), .. } => {
                self.selection = Some(id);
                // Drawing tools stay in hand: the user is usually drawing more
                // than one arrow, and jumping back to Select after each would
                // make the second one a re-pick every time.
            }
            Drag::Drawing { id, points } => {
                if points.len() < 2 {
                    self.document.remove(id);
                }
            }
            Drag::Cropping { origin, current } => {
                let rect = LogicalRect::from_corners(origin, current);
                if rect.size.width >= MIN_SIZE && rect.size.height >= MIN_SIZE {
                    let _ = self.document.set_crop(Some(rect));
                    self.tool = Tool::Select;
                }
            }
            _ => {}
        }
        self.commit();
        self.history.seal();
        self.touch();
    }

    /// Begins panning from a screen-space grab point.
    pub fn begin_pan(&mut self, at: (f32, f32)) {
        self.drag = Drag::Panning {
            grab: at,
            start: self.pan,
        };
    }

    /// Continues a pan to a screen-space point.
    pub fn pan_to(&mut self, at: (f32, f32)) {
        if let Drag::Panning { grab, start } = self.drag {
            self.pan = (start.0 + at.0 - grab.0, start.1 + at.1 - grab.1);
            self.touch();
        }
    }

    /// Abandons any gesture in progress, undoing what it created.
    pub fn cancel_drag(&mut self) {
        match std::mem::replace(&mut self.drag, Drag::Idle) {
            Drag::Creating { id: Some(id), .. } | Drag::Drawing { id, .. } => {
                self.document.remove(id);
                if self.selection == Some(id) {
                    self.selection = None;
                }
            }
            Drag::Moving { start, .. } => {
                if let Some(id) = self.selection {
                    self.document.set_bounds(id, start);
                }
            }
            Drag::Resizing { start, .. } => {
                if let Some(id) = self.selection {
                    self.document.set_bounds(id, start);
                }
            }
            _ => {}
        }
        self.history.seal();
        self.touch();
    }

    /// The handle under `point`, if the selection has one there.
    #[must_use]
    pub fn handle_at(&self, point: LogicalPoint) -> Option<Handle> {
        let bounds = self.selection_bounds()?;
        Handle::ALL
            .into_iter()
            .find(|h| geom::distance(h.position(&bounds), point) <= HANDLE_TOLERANCE)
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    /// Runs a command, reporting what the host should do next.
    ///
    /// Every arm but [`Command::Copy`] and [`Command::Save`] bumps the
    /// revision: those two only ask the host for something and change nothing,
    /// so bumping would throw away a preview texture that is still correct.
    ///
    /// # Errors
    ///
    /// Returns an error if an undo or redo could not be applied. The state is
    /// left usable either way.
    pub fn command(&mut self, command: Command) -> Result<Intent> {
        let intent = match command {
            Command::Escape => self.escape(),
            Command::Delete => {
                if let Some(id) = self.selection.take() {
                    self.document.remove(id);
                    self.editing_text = None;
                    self.commit();
                } else if self.document.crop().is_some() {
                    // With nothing selected, delete clears the crop: it is the
                    // only other thing on screen that can be "removed".
                    let _ = self.document.set_crop(None);
                    self.commit();
                }
                Intent::None
            }
            Command::Undo => {
                self.cancel_drag();
                self.history.undo(&mut self.document)?;
                self.after_history();
                Intent::None
            }
            Command::Redo => {
                self.cancel_drag();
                self.history.redo(&mut self.document)?;
                self.after_history();
                Intent::None
            }
            Command::Copy => return Ok(Intent::Copy),
            Command::Save => return Ok(Intent::Save),
            Command::SelectAll => {
                self.selection = self.document.annotations().last().map(|o| o.id);
                Intent::None
            }
            Command::Pick(tool) => {
                self.set_tool(tool);
                Intent::None
            }
            Command::Nudge { dx, dy } => {
                if let Some(id) = self.selection {
                    self.document.translate(id, dx, dy);
                    self.commit_coalesced("nudge");
                }
                Intent::None
            }
            Command::ZoomIn => {
                self.set_zoom(self.zoom * ZOOM_STEP);
                Intent::None
            }
            Command::ZoomOut => {
                self.set_zoom(self.zoom / ZOOM_STEP);
                Intent::None
            }
            Command::ZoomReset => {
                self.set_zoom(1.0);
                self.set_pan((0.0, 0.0));
                Intent::None
            }
            Command::ApplyCrop => {
                if let Some(rect) = self.pending_crop() {
                    self.drag = Drag::Idle;
                    let _ = self.document.set_crop(Some(rect));
                    self.tool = Tool::Select;
                    self.commit();
                }
                Intent::None
            }
            Command::SendToBack => {
                if let Some(id) = self.selection {
                    self.document.send_to_back(id);
                    self.commit();
                }
                Intent::None
            }
            Command::BringToFront => {
                if let Some(id) = self.selection {
                    self.document.bring_to_front(id);
                    self.commit();
                }
                Intent::None
            }
        };
        self.touch();
        Ok(intent)
    }

    /// Escape unwinds one layer of state at a time, closing only once there is
    /// nothing left to back out of. Closing straight away would throw away a
    /// half-drawn shape *and* the window, when the user meant only the shape.
    fn escape(&mut self) -> Intent {
        if self.editing_text.is_some() {
            self.finish_text();
        } else if self.is_dragging() {
            self.cancel_drag();
        } else if self.selection.is_some() {
            self.select(None);
        } else if self.tool == Tool::Select {
            return Intent::Close;
        } else {
            self.set_tool(Tool::Select);
        }
        Intent::None
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    fn press_select(&mut self, point: LogicalPoint) {
        if let Some(handle) = self.handle_at(point)
            && let Some(start) = self.selection_bounds()
        {
            self.drag = Drag::Resizing {
                handle,
                grab: point,
                start,
            };
            return;
        }
        match self.document.hit_test(point) {
            Some(id) => {
                self.selection = Some(id);
                if let Some(object) = self.document.get(id) {
                    self.style = object.style;
                    let is_text = matches!(object.annotation, Annotation::Text { .. });
                    let bounds = object.bounds();
                    if is_text {
                        self.editing_text = Some(id);
                    }
                    self.drag = Drag::Moving {
                        grab: point,
                        start: bounds,
                    };
                }
            }
            None => self.selection = None,
        }
    }

    fn place(&mut self, tool: Tool, point: LogicalPoint) {
        let id = match tool {
            Tool::Text => {
                let id = self.document.add(
                    Annotation::Text {
                        at: point,
                        content: String::new(),
                    },
                    self.style,
                );
                self.editing_text = Some(id);
                id
            }
            Tool::Counter => {
                // The document renumbers counters itself, so the index handed
                // in is only a placeholder for "one past the last".
                let index = self.document.counter_count().saturating_add(1);
                self.document.add(
                    Annotation::Counter { at: point, index },
                    self.style.with_fill(Some(self.style.stroke)),
                )
            }
            _ => return,
        };
        self.selection = Some(id);
        self.commit();
    }

    /// Grows the annotation being dragged out, creating it on the first move
    /// that clears [`MIN_DRAG`].
    fn grow(
        &mut self,
        origin: LogicalPoint,
        point: LogicalPoint,
        existing: Option<AnnotationId>,
    ) -> Option<AnnotationId> {
        if existing.is_none() && geom::distance(origin, point) < MIN_DRAG {
            return None;
        }
        let annotation = match self.tool {
            Tool::Arrow => Annotation::Arrow {
                from: origin,
                to: point,
            },
            Tool::Line => Annotation::Line {
                from: origin,
                to: point,
            },
            Tool::Rectangle => Annotation::Rectangle(LogicalRect::from_corners(origin, point)),
            Tool::Ellipse => Annotation::Ellipse(LogicalRect::from_corners(origin, point)),
            Tool::Highlight => Annotation::Highlight(LogicalRect::from_corners(origin, point)),
            Tool::Blur => Annotation::Redact {
                area: LogicalRect::from_corners(origin, point),
                style: RedactStyle::Blur,
            },
            Tool::Pixelate => Annotation::Redact {
                area: LogicalRect::from_corners(origin, point),
                style: RedactStyle::Pixelate,
            },
            _ => return existing,
        };
        match existing {
            Some(id) => {
                if let Some(mut object) = self.document.get_mut(id) {
                    *object.annotation() = annotation;
                }
                Some(id)
            }
            None => Some(self.document.add(annotation, self.style_for_new())),
        }
    }

    /// The style a newly created annotation gets.
    ///
    /// Highlight and redaction ignore the palette: a highlighter drawn in
    /// opaque red would obscure what it is meant to emphasise, and a redaction
    /// has no user-facing colour at all.
    fn style_for_new(&self) -> Style {
        match self.tool {
            Tool::Highlight => Style::highlighter(),
            Tool::Blur | Tool::Pixelate => Style::redaction(),
            _ => self.style,
        }
    }

    fn commit(&mut self) {
        let before = self.history.undo_depth();
        self.history.commit(&self.document);
        if self.history.undo_depth() != before {
            self.dirty = true;
        }
    }

    fn commit_coalesced(&mut self, tag: &str) {
        let before = self.history.undo_depth();
        self.history.commit_coalesced(&self.document, tag);
        if self.history.undo_depth() != before {
            self.dirty = true;
        }
    }

    /// Drops references to annotations that an undo or redo removed.
    fn after_history(&mut self) {
        if self
            .selection
            .is_some_and(|id| self.document.get(id).is_none())
        {
            self.selection = None;
        }
        if self
            .editing_text
            .is_some_and(|id| self.document.get(id).is_none())
        {
            self.editing_text = None;
        }
        self.dirty = true;
    }

    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

/// The style to apply to an existing annotation when the palette changes.
///
/// A highlighter that adopted a solid opaque stroke would stop being a
/// highlighter, and a redaction has no user-facing colour at all, so both keep
/// most of what they were built with.
fn restyle_for(document: &Document, id: AnnotationId, style: Style) -> Style {
    let Some(object) = document.get(id) else {
        return style;
    };
    match object.annotation.kind() {
        AnnotationKind::Redact => object.style,
        AnnotationKind::Highlight => object
            .style
            .with_stroke(style.stroke)
            .with_fill(Some(style.stroke)),
        _ => style,
    }
}

/// Keeps a resized rectangle from collapsing to nothing.
fn enforce_min(rect: LogicalRect) -> LogicalRect {
    LogicalRect::new(
        rect.origin,
        LogicalSize::new(
            rect.size.width.max(MIN_SIZE),
            rect.size.height.max(MIN_SIZE),
        ),
    )
}

/// Snaps a free corner so the rectangle it spans is square.
///
/// Distinct from [`constrain_to_axis`]: applying that to a rectangle drag
/// collapses it to a zero-height sliver, because an axis-locked *corner* means
/// an empty rectangle rather than a straight line.
fn square_from(origin: LogicalPoint, point: LogicalPoint) -> LogicalPoint {
    let (dx, dy) = (point.x - origin.x, point.y - origin.y);
    let side = dx.abs().max(dy.abs());
    LogicalPoint::new(origin.x + side.copysign(dx), origin.y + side.copysign(dy))
}

/// Snaps a free endpoint to the nearest of horizontal, vertical or 45°.
fn constrain_to_axis(origin: LogicalPoint, point: LogicalPoint) -> LogicalPoint {
    let (dx, dy) = (point.x - origin.x, point.y - origin.y);
    let (ax, ay) = (dx.abs(), dy.abs());
    // Within this band of the diagonal, snap to it; outside, snap to an axis.
    if (ax - ay).abs() < ax.max(ay) * 0.35 {
        let d = ax.max(ay);
        LogicalPoint::new(origin.x + d.copysign(dx), origin.y + d.copysign(dy))
    } else if ax > ay {
        LogicalPoint::new(point.x, origin.y)
    } else {
        LogicalPoint::new(origin.x, point.y)
    }
}

/// Restricts a translation to whichever axis it has travelled furthest along.
fn axis_lock(dx: f64, dy: f64) -> (f64, f64) {
    if dx.abs() >= dy.abs() {
        (dx, 0.0)
    } else {
        (0.0, dy)
    }
}
