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
    Annotation, AnnotationId, AnnotationKind, ArrowStyle, Color, Document, History, RedactStyle,
    Style, geom,
};
use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize, Result};
use std::ops::Range;

/// How close, in *screen* points, a pointer must come to a resize handle.
///
/// Screen rather than document, because it describes reaching a control drawn
/// at a fixed on-screen size. [`EditorState::handle_at`] divides it by the
/// current view scale to compare against a document-space point.
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

/// Screen-point reach for snapping a crop edge to the source boundary.
pub const CROP_SNAP_TOLERANCE: f64 = 8.0;

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
    /// The geometric start of an arrow.
    ArrowStart,
    /// The geometric tip of an arrow.
    ArrowEnd,
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
    /// Every rectangular handle, clockwise from the top-left.
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

    /// The two handles an arrow exposes.
    pub const ARROW: [Self; 2] = [Self::ArrowStart, Self::ArrowEnd];

    /// Where this handle sits on a rectangle.
    #[must_use]
    pub fn position(self, rect: &LogicalRect) -> LogicalPoint {
        let (l, t) = (rect.origin.x, rect.origin.y);
        let (r, b) = (geom::max_x(rect), geom::max_y(rect));
        let (cx, cy) = ((l + r) / 2.0, (t + b) / 2.0);
        match self {
            Self::ArrowStart => LogicalPoint::new(l, t),
            Self::ArrowEnd => LogicalPoint::new(r, b),
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
        if matches!(self, Self::ArrowStart | Self::ArrowEnd) {
            return *rect;
        }
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

/// Aspect constraint for the dedicated crop mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CropAspect {
    /// Each edge moves independently.
    #[default]
    Freeform,
    /// Match the source image.
    Original,
    /// Equal width and height.
    Square,
    /// Landscape 16:9.
    Landscape16x9,
    /// Portrait 9:16.
    Portrait9x16,
    /// Landscape 4:3.
    Landscape4x3,
    /// Portrait 3:4.
    Portrait3x4,
}

impl CropAspect {
    /// Presets in menu order.
    pub const ALL: [Self; 7] = [
        Self::Freeform,
        Self::Original,
        Self::Square,
        Self::Landscape16x9,
        Self::Portrait9x16,
        Self::Landscape4x3,
        Self::Portrait3x4,
    ];

    /// User-facing preset name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Freeform => "Freeform",
            Self::Original => "Original",
            Self::Square => "Square",
            Self::Landscape16x9 => "16:9",
            Self::Portrait9x16 => "9:16",
            Self::Landscape4x3 => "4:3",
            Self::Portrait3x4 => "3:4",
        }
    }

    fn ratio(self, source: LogicalSize) -> Option<f64> {
        match self {
            Self::Freeform => None,
            Self::Original => (source.height > 0.0).then_some(source.width / source.height),
            Self::Square => Some(1.0),
            Self::Landscape16x9 => Some(16.0 / 9.0),
            Self::Portrait9x16 => Some(9.0 / 16.0),
            Self::Landscape4x3 => Some(4.0 / 3.0),
            Self::Portrait3x4 => Some(3.0 / 4.0),
        }
    }

    fn swapped(self) -> Self {
        match self {
            Self::Landscape16x9 => Self::Portrait9x16,
            Self::Portrait9x16 => Self::Landscape16x9,
            Self::Landscape4x3 => Self::Portrait3x4,
            Self::Portrait3x4 => Self::Landscape4x3,
            other => other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CropSession {
    rect: LogicalRect,
    aspect: CropAspect,
    snap_edges: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CropDrag {
    Move,
    Resize(Handle),
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
    ArrowEndpoint {
        handle: Handle,
        from: LogicalPoint,
        to: LogicalPoint,
    },
    ArrowBend {
        from: LogicalPoint,
        to: LogicalPoint,
        bend: f64,
    },
    CropAdjust {
        action: CropDrag,
        grab: LogicalPoint,
        start: LogicalRect,
    },
    Panning {
        grab: (f32, f32),
        start: (f32, f32),
    },
}

/// Where to move the text caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caret {
    /// One character left.
    Left,
    /// One character right.
    Right,
    /// Same column, previous line.
    Up,
    /// Same column, next line.
    Down,
    /// The start of the current line.
    LineStart,
    /// The end of the current line.
    LineEnd,
}

/// One editing operation on the text annotation being typed into.
///
/// Separate from [`Command`] because these carry text: an accelerator table
/// stays `Copy` and comparable, while input that arrives as a string does not
/// have to pretend to be a keystroke. [`Insert`](TextEdit::Insert) is what a
/// keypress or a finished IME composition produces;
/// [`Preedit`](TextEdit::Preedit) is composition still in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextEdit {
    /// Insert committed text at the caret, replacing any composition.
    Insert(String),
    /// Replace the composition in progress with `0`, which may be empty.
    ///
    /// Empty means the IME was dismissed, so the composition is simply removed.
    Preedit(String),
    /// Delete the character before the caret.
    Backspace,
    /// Delete the character after the caret.
    DeleteForward,
    /// Move the caret without changing the text.
    Caret(Caret),
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
    /// Leave crop mode without changing the document.
    CancelCrop,
    /// Clear the committed crop and return to the original image bounds.
    RevertCrop,
    /// Send the selection behind everything else.
    SendToBack,
    /// Bring the selection in front of everything else.
    BringToFront,
}

impl Command {
    /// Whether this command has to settle an unfinished label before it runs.
    ///
    /// A label the user has not finished holds a rollback point: the click that
    /// placed it, which Escape can still take back as though it never happened.
    /// Anything committed while that point is open lands *above* it, so
    /// cancelling the label afterwards rolls that work back as well — silently,
    /// with nothing left to redo. Nudging a selection while a set-aside label
    /// was pending used to lose the nudge outright that way.
    ///
    /// So every command that changes the document, the history, or which
    /// annotation the next such command will act on settles the label first.
    /// The match is exhaustive on purpose: a command added later cannot slip
    /// through without someone deciding which side of this line it belongs on.
    ///
    /// [`Undo`](Self::Undo) and [`Redo`](Self::Redo) are deliberately outside
    /// it. Navigating the history is *how* a label gets set aside and picked up
    /// again; settling on the way past would make undo cancel the very label it
    /// exists to be able to bring back. [`Escape`](Self::Escape) is outside it
    /// too, because settling is the first thing it does itself and it has to be
    /// able to carry on when that is refused.
    const fn settles_unfinished_text(self) -> bool {
        match self {
            Self::Delete
            | Self::SelectAll
            | Self::Nudge { .. }
            | Self::ApplyCrop
            | Self::RevertCrop
            | Self::SendToBack
            | Self::BringToFront => true,
            Self::Escape
            | Self::Undo
            | Self::Redo
            | Self::Copy
            | Self::Save
            | Self::Pick(_)
            | Self::ZoomIn
            | Self::ZoomOut
            | Self::ZoomReset
            | Self::CancelCrop => false,
        }
    }
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
    /// Open the platform's custom colour picker.
    CustomColor,
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
    /// A label that was being edited until an undo took it out of the document.
    ///
    /// Editing cannot continue while the annotation does not exist, but the
    /// undo that removed it is redoable, and a redo brings back a label that is
    /// empty and invisible. Without somewhere to remember it, that label would
    /// come back with nobody editing it: unreachable, uncancellable, and
    /// exactly the empty annotation [`finish_text`](Self::finish_text) exists
    /// to prevent. Held here so a redo can put the user back where they were.
    suspended_text: Option<AnnotationId>,
    /// Whether [`editing_text`](Self::editing_text) was created by the click
    /// that opened it. See [`EditorState::finish_text`].
    text_is_new: bool,
    /// Whether the platform must be told to drop any composition in flight.
    ///
    /// Clearing our own idea of the preedit is not enough. The IME lives in
    /// another process and still believes it is composing; the next Preedit or
    /// Commit it sends would splice glyphs from an undone edit into text that no
    /// longer has room for them. egui carries the instruction in `IMEOutput`,
    /// and it has to be sent exactly once — repeating it would cancel every
    /// composition the user starts afterwards.
    interrupt_ime: bool,
    caret: usize,
    preedit: Option<Range<usize>>,
    crop: Option<CropSession>,
    zoom: f32,
    fit_zoom: bool,
    pan: (f32, f32),
    view_scale: f64,
    revision: u64,
    view_revision: u64,
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
            suspended_text: None,
            text_is_new: false,
            interrupt_ime: false,
            caret: 0,
            preedit: None,
            crop: None,
            zoom: 1.0,
            fit_zoom: true,
            pan: (0.0, 0.0),
            view_scale: 1.0,
            revision: 0,
            view_revision: 0,
            dirty: false,
        }
    }

    /// The document being edited.
    #[must_use]
    pub const fn document(&self) -> &Document {
        &self.document
    }

    /// A counter that changes whenever the rendered *content* would change.
    ///
    /// The painter caches its preview texture against this rather than
    /// re-rendering every frame: a full composite of a 5K capture is far too
    /// slow for 60 fps, and re-doing it when nothing moved would make the
    /// editor unusable on exactly the large captures it matters for.
    ///
    /// Deliberately *not* bumped by zoom and pan. Moving the viewport does not
    /// change a single pixel of the composite, and a pan is a continuous
    /// gesture: bumping this would re-rasterise and re-upload a 2400 px texture
    /// on every frame of a drag, which is precisely the stall the cache exists
    /// to avoid. Zoom can change the *resolution* the preview wants, but the
    /// painter already keys on its quantised target width for that, so zoom
    /// does not belong here either. Use [`EditorState::view_revision`] to
    /// observe viewport movement.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// A counter that changes whenever the viewport moves.
    ///
    /// Zoom and pan only, never content. Separate from
    /// [`EditorState::revision`] so a caller can tell "the picture changed"
    /// from "the camera moved" — the first needs a new composite, the second
    /// needs nothing but a different destination rectangle.
    #[must_use]
    pub const fn view_revision(&self) -> u64 {
        self.view_revision
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
        // Best effort. Picking a tool commits nothing, so a label the history
        // will not take back can stay pending without anything landing on top
        // of it — and refusing the tool change would strand the user instead.
        let _ = self.settle_text();
        if tool == Tool::Crop {
            self.crop = Some(CropSession {
                rect: self
                    .document
                    .crop()
                    .unwrap_or_else(|| self.document.logical_bounds()),
                aspect: CropAspect::Freeform,
                snap_edges: true,
            });
        } else {
            self.crop = None;
        }
        self.tool = tool;
        // Arrow is the one drawing tool that also edits its own existing
        // objects. Every other drawing tool clears selection because its handles
        // would be controls the active tool cannot use.
        let keeps_arrow = tool == Tool::Arrow
            && self
                .selection
                .and_then(|id| self.document.get(id))
                .is_some_and(|object| matches!(object.annotation, Annotation::Arrow { .. }));
        if tool != Tool::Select && !keeps_arrow {
            self.selection = None;
        }
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
            let changed = self
                .document
                .get(id)
                .is_some_and(|object| object.style != restyled);
            if changed && self.document.set_style(id, restyled) {
                self.commit_coalesced("style");
                self.touch();
            }
        }
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

    /// Arrow shape language currently in hand.
    #[must_use]
    pub const fn arrow_style(&self) -> ArrowStyle {
        self.style.arrow_style
    }

    /// Sets the arrow shape language for the selection and future arrows.
    pub fn set_arrow_style(&mut self, arrow_style: ArrowStyle) {
        let mut style = self.style.with_arrow_style(arrow_style);
        if arrow_style == ArrowStyle::Curved && style.effective_arrow_bend().abs() < f64::EPSILON {
            style = style.with_arrow_bend(0.28);
        } else if arrow_style != ArrowStyle::Curved && self.style.arrow_style != arrow_style {
            style = style.with_arrow_bend(0.0);
        }
        self.set_style(style);
    }

    /// Signed arrow bend in source-relative units.
    #[must_use]
    pub fn arrow_bend(&self) -> f64 {
        self.style.effective_arrow_bend()
    }

    /// Sets the bend for the selection and future arrows.
    pub fn set_arrow_bend(&mut self, bend: f64) {
        self.set_style(self.style.with_arrow_bend(bend.clamp(-0.75, 0.75)));
    }

    /// The selected annotation, if any.
    #[must_use]
    pub const fn selection(&self) -> Option<AnnotationId> {
        self.selection
    }

    /// Whether the current selection is an arrow.
    #[must_use]
    pub fn selection_is_arrow(&self) -> bool {
        self.selection
            .and_then(|id| self.document.get(id))
            .is_some_and(|object| matches!(object.annotation, Annotation::Arrow { .. }))
    }

    /// The selection's bounding box, if there is a selection.
    #[must_use]
    pub fn selection_bounds(&self) -> Option<LogicalRect> {
        self.selection
            .and_then(|id| self.document.get(id))
            .map(scrozz_annotate::AnnotationObject::bounds)
    }

    /// The editor-chrome handles for the current selection.
    ///
    /// Arrows expose exactly their two geometric endpoints. Every other
    /// resizable annotation uses the rectangular handle family.
    #[must_use]
    pub fn selection_handles(&self) -> Vec<(Handle, LogicalPoint)> {
        let Some(object) = self.selection.and_then(|id| self.document.get(id)) else {
            return Vec::new();
        };
        if let Annotation::Arrow { from, to } = &object.annotation {
            return vec![(Handle::ArrowStart, *from), (Handle::ArrowEnd, *to)];
        }
        let bounds = object.bounds();
        Handle::ALL
            .into_iter()
            .map(|handle| (handle, handle.position(&bounds)))
            .collect()
    }

    /// Distinct diamond bend affordance for a selected arrow.
    #[must_use]
    pub fn arrow_bend_handle(&self) -> Option<LogicalPoint> {
        self.selection
            .and_then(|id| self.document.get(id))
            .and_then(scrozz_annotate::AnnotationObject::arrow_bend_handle)
    }

    /// Selects an annotation, or clears the selection with `None`.
    pub fn select(&mut self, id: Option<AnnotationId>) {
        if self.selection == id {
            return;
        }
        self.selection = id.filter(|id| self.document.get(*id).is_some());
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

    /// The caret's position in the text being edited, as a byte offset.
    ///
    /// Byte rather than character offset because it indexes the content string
    /// directly; every mutation keeps it on a `char` boundary.
    #[must_use]
    pub const fn text_caret(&self) -> usize {
        self.caret
    }

    /// The caret's row and column, in characters, for drawing it.
    ///
    /// The built-in font is monospaced, so a column translates to an x offset
    /// by multiplying by the advance — which is why this is the useful shape to
    /// return rather than a pixel position the state has no business computing.
    #[must_use]
    pub fn text_caret_cell(&self) -> Option<(usize, usize)> {
        let text = self.text_buffer()?;
        let before = text.get(..self.caret.min(text.len()))?;
        let row = before.matches('\n').count();
        let col = before
            .rsplit('\n')
            .next()
            .map_or(0, |line| line.chars().count());
        Some((row, col))
    }

    /// The byte range of the IME composition in progress, if any.
    #[must_use]
    pub fn preedit(&self) -> Option<Range<usize>> {
        self.preedit.clone()
    }

    /// Applies one text-editing operation to the annotation being typed into.
    ///
    /// A no-op when no text annotation is being edited, so a stray keystroke
    /// after a commit cannot resurrect one.
    pub fn text_edit(&mut self, edit: &TextEdit) {
        if self.editing_text.is_none() {
            return;
        }
        let Some(text) = self.text_buffer().map(str::to_owned) else {
            return;
        };
        let caret = self.caret.min(text.len());
        let preedit = self
            .preedit
            .clone()
            .filter(|r| r.start <= text.len() && r.end <= text.len() && r.start <= r.end);

        let (next, caret, preedit) = match edit {
            TextEdit::Insert(insert) => {
                let range = preedit.unwrap_or(caret..caret);
                let mut next = text.clone();
                next.replace_range(range.clone(), insert);
                (next, range.start + insert.len(), None)
            }
            TextEdit::Preedit(composition) => {
                let range = preedit.unwrap_or(caret..caret);
                let mut next = text.clone();
                next.replace_range(range.clone(), composition);
                let end = range.start + composition.len();
                let still = (!composition.is_empty()).then_some(range.start..end);
                (next, end, still)
            }
            TextEdit::Backspace => {
                // While composing, the platform IME owns backspace and sends a
                // shorter preedit; this path is for ordinary typing.
                let Some(prev) = prev_boundary(&text, caret) else {
                    return;
                };
                let mut next = text.clone();
                next.replace_range(prev..caret, "");
                (next, prev, shift_preedit(preedit, prev, caret))
            }
            TextEdit::DeleteForward => {
                let Some(following) = next_boundary(&text, caret) else {
                    return;
                };
                let mut next = text.clone();
                next.replace_range(caret..following, "");
                (next, caret, shift_preedit(preedit, caret, following))
            }
            TextEdit::Caret(motion) => {
                // Moving the caret ends any composition in place: the glyphs
                // stay, they simply stop being provisional.
                (text.clone(), move_caret(&text, caret, *motion), None)
            }
        };

        self.set_text_buffer(&next);
        self.caret = caret.min(next.len());
        self.mark_caret();
        self.preedit = preedit;
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
        // Clamp rather than reset: a caller replacing the whole buffer must not
        // be able to leave the caret pointing past the end, or into the middle
        // of a multi-byte character.
        self.caret = clamp_boundary(text, self.caret);
        if changed {
            self.commit_coalesced("text");
            self.touch();
        }
    }

    /// Starts typing into `id`, with the caret after whatever is already there.
    ///
    /// `fresh` says whether `id` was created by the click that started this —
    /// see [`finish_text`](Self::finish_text) for why that distinction matters.
    ///
    /// # Why this must never run over a pending label
    ///
    /// A label that has been set aside is still holding the rollback point for
    /// the click that placed it, and the id in `suspended_text` is the only
    /// thing that will pick it back up when a redo returns it to the document.
    /// Starting another editing session over the top of that drops the id while
    /// leaving the rollback point open, so the click stays on the redo stack
    /// with nobody holding it — the same ghost
    /// [`finish_suspended_text`](Self::finish_suspended_text) exists to prevent,
    /// reached from the other end. Callers settle first, and refuse the press
    /// when settling is refused; the assertion is here so that stays true of
    /// callers added later.
    fn begin_text(&mut self, id: AnnotationId, fresh: bool) {
        debug_assert!(
            self.suspended_text.is_none(),
            "a label the user has not finished is still waiting to come back; \
             starting a second editing session over it strands the first"
        );
        self.editing_text = Some(id);
        self.text_is_new = fresh;
        self.caret = self.text_buffer().map_or(0, str::len);
        self.preedit = None;
        self.mark_caret();
    }

    /// Finishes text entry, removing the annotation if nothing was typed.
    ///
    /// An empty text annotation is invisible but selectable, which reads as a
    /// bug: clicking to place text and then thinking better of it must leave
    /// nothing behind.
    ///
    /// # Why abandoning differs from deleting
    ///
    /// Placing a label and typing nothing must leave the undo stack exactly as
    /// it was. Removing the annotation and committing would not do that: the
    /// step that *created* it is still on the stack, so one ⌘Z brings back an
    /// invisible empty label the user never made. That group is discarded
    /// instead, as though the click had not happened.
    ///
    /// Emptying a label that already existed is the opposite case — the user
    /// deleted text that was really there, and undo must bring it back — so that
    /// one commits normally. `text_is_new` is what tells them apart.
    ///
    /// # Why a label nobody is editing can still need finishing
    ///
    /// Undoing the click that placed a label takes it out of the document, so
    /// there is nothing to edit and the editing session is set aside rather than
    /// ended — the undo is redoable, and a redo has to put the user back in the
    /// label they were typing into. Walking away at that moment is still walking
    /// away from an unfinished label, and the click that made it still has to be
    /// taken back. Forgetting it here instead left the creation on the redo
    /// stack with nothing holding its editing session, so redoing produced an
    /// empty label that could not be seen, typed into, or escaped from.
    ///
    /// Returns whether the unfinished label was dealt with. Only one case
    /// answers `false`: a set-aside label whose placement the history will not
    /// take back. It stays pending — the alternative is a ghost — so the caller
    /// must be able to tell that its request went unserved and carry on rather
    /// than assume the state is now clean.
    pub fn finish_text(&mut self) -> bool {
        // Deliberately before the `take` below: the set-aside label has no
        // `editing_text` to find, and clearing it first is how it was lost.
        if self.editing_text.is_none()
            && let Some(id) = self.suspended_text
        {
            return self.finish_suspended_text(id);
        }
        self.suspended_text = None;
        let Some(id) = self.editing_text.take() else {
            return true;
        };
        let fresh = std::mem::take(&mut self.text_is_new);
        // Any composition in flight becomes ordinary text. Dropping it would
        // discard glyphs the user can see, which is never what leaving a field
        // means on any platform.
        self.preedit = None;
        self.caret = 0;
        self.mark_caret();
        let empty = matches!(
            self.document.get(id).map(|o| &o.annotation),
            Some(Annotation::Text { content, .. }) if content.trim().is_empty()
        );
        let mut changed = false;
        if empty {
            if self.selection == Some(id) {
                self.selection = None;
            }
            if fresh
                && self
                    .history
                    .abandon(&mut self.document)
                    .expect("the document has not changed provenance mid-edit")
            {
                self.touch();
                return true;
            }
            changed = self.document.remove(id).is_some();
        }
        // Whatever the label turned out to be, it is now the user's: there is
        // no longer a click to take back.
        self.history.finish();
        self.history.seal();
        self.commit();
        if changed {
            self.touch();
        }
        true
    }

    /// Ends an editing session whose annotation an undo took out of the
    /// document.
    ///
    /// A label the user placed and then undid their way out of is cancelled
    /// outright, which also drops the redo branch that would have brought it
    /// back — the click is taken back, so there is nothing left to redo it into.
    ///
    /// When the rollback is refused, the cancellation is left pending instead of
    /// being faked. Refusing means the history has moved somewhere the creation
    /// can no longer be lifted out of; closing the group here would leave the
    /// creation on the redo stack with nobody holding its editing session, which
    /// is the ghost this exists to prevent. Left open, a redo puts the user back
    /// in the label and the next Escape cancels it properly.
    ///
    /// # Why a refusal cannot be waited out forever
    ///
    /// Waiting only makes sense while there is something to wait for. Once the
    /// click that placed the label has gone from the history for good — because
    /// work committed since truncated the branch it sat in, or because it fell
    /// off the back of a full history — no redo will ever bring the label back,
    /// so there is no ghost left to guard against and nothing left to cancel.
    /// Holding on past that point was its own bug: the pending label could never
    /// be settled, and every later attempt to place one was refused on its
    /// behalf, which left the text tool dead for the rest of the session. So the
    /// stale rollback point is closed and the label let go of. The document is
    /// not touched — the label is already out of it, and the history says it is
    /// not coming back.
    ///
    /// Returns whether it was dealt with, so a refusal does not read as success.
    /// A caller that keeps asking the same question and keeps being told nothing
    /// happened has to be free to do something else — Escape that only ever
    /// retried a rollback the history had already ruled out could not reset the
    /// tool or close the editor, and there is no key that undoes being stuck.
    fn finish_suspended_text(&mut self, id: AnnotationId) -> bool {
        if !self.text_is_new {
            // A label that was already there before this session. It is not this
            // editor's to un-create, and it is visible when it comes back.
            self.suspended_text = None;
            self.preedit = None;
            return true;
        }
        let cancelled = self
            .history
            .abandon(&mut self.document)
            .expect("the document has not changed provenance mid-edit");
        if !cancelled {
            if self.history.abandon_is_still_reachable() {
                return false;
            }
            self.history.finish();
        }
        self.suspended_text = None;
        self.text_is_new = false;
        self.preedit = None;
        self.caret = 0;
        if self.selection == Some(id) {
            self.selection = None;
        }
        self.mark_caret();
        true
    }

    /// Settles a label the user has not finished, before work that is not part
    /// of it.
    ///
    /// This is the one gate for the rule that an unfinished label is dealt with
    /// before anything else is committed — see
    /// [`Command::settles_unfinished_text`] for why that rule exists.
    ///
    /// # What a refusal means
    ///
    /// `false` says the history has moved somewhere the label's placement can
    /// no longer be lifted out of *yet*, so the label stays pending rather than
    /// being faked away. Work committed after that is safe: it lands above a
    /// rollback point that has already been ruled out, and the cancellation
    /// stays refused, so nothing the user does next can be rolled back
    /// underneath them.
    ///
    /// What is not safe is beginning a second editing session, whether by
    /// placing a new label or by clicking into one that is already there. The
    /// pending label's id is the only thing that will pick it back up when a
    /// redo returns it, and its rollback point is still open; starting another
    /// session drops the id and leaves the click stranded in a branch nothing is
    /// holding, so redoing far enough put an invisible, unselectable,
    /// uncancellable label back in the document.
    /// [`press_opens_a_label`](Self::press_opens_a_label) is where that is
    /// refused, for both routes.
    ///
    /// The state is rare and recoverable: one Redo returns to the label and
    /// hands it back to the user, and Escape still unwinds normally because it
    /// does not stop at this gate. And if the placement is ever put beyond
    /// recovery — by work that truncates the branch it sat in — the label is let
    /// go of rather than waited on forever, which is what
    /// [`finish_suspended_text`](Self::finish_suspended_text) decides.
    fn settle_text(&mut self) -> bool {
        if self.editing_text.is_none() && self.suspended_text.is_none() {
            return true;
        }
        self.finish_text()
    }

    /// The current manual zoom factor.
    ///
    /// While Fit is active, [`Self::view_scale`] is the live fitted percentage
    /// and this stored value remains the last neutral default.
    #[must_use]
    pub const fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Whether the image is fitted to the current viewport.
    #[must_use]
    pub const fn is_fit_zoom(&self) -> bool {
        self.fit_zoom
    }

    /// Effective zoom for the next gesture or key step.
    #[must_use]
    pub fn effective_zoom(&self) -> f32 {
        if self.fit_zoom {
            self.view_scale as f32
        } else {
            self.zoom
        }
    }

    /// Sets an absolute manual zoom, clamped to the supported range.
    pub fn set_zoom(&mut self, zoom: f32) {
        let clamped = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if clamped.is_finite() && (self.fit_zoom || (clamped - self.zoom).abs() > f32::EPSILON) {
            self.zoom = clamped;
            self.fit_zoom = false;
            self.touch_view();
        }
    }

    /// Sets zoom around an anchor measured from the viewport centre.
    ///
    /// Keeping the anchor's document point stationary is a pure pan adjustment:
    /// the vector from the image centre to the anchor scales with the zoom.
    pub fn zoom_about(&mut self, zoom: f32, anchor: (f32, f32)) {
        let next = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let current = self.effective_zoom();
        if !next.is_finite()
            || (!self.fit_zoom && (next - current).abs() <= f32::EPSILON)
            || current <= 0.0
        {
            return;
        }
        let ratio = next / current;
        self.pan = (
            anchor.0 - (anchor.0 - self.pan.0) * ratio,
            anchor.1 - (anchor.1 - self.pan.1) * ratio,
        );
        self.zoom = next;
        self.fit_zoom = false;
        self.touch_view();
    }

    /// The pan offset, in screen points.
    #[must_use]
    pub const fn pan(&self) -> (f32, f32) {
        self.pan
    }

    /// Sets the pan offset.
    pub fn set_pan(&mut self, pan: (f32, f32)) {
        if (pan.0 - self.pan.0).abs() <= f32::EPSILON && (pan.1 - self.pan.1).abs() <= f32::EPSILON
        {
            return;
        }
        self.pan = pan;
        self.touch_view();
    }

    /// How many screen points one document point currently occupies.
    ///
    /// The painter feeds this back each frame. The state needs it for exactly
    /// one thing — sizing hit-test tolerances that are specified in screen
    /// points — and taking it as a value keeps the state free of any other
    /// knowledge of the view.
    #[must_use]
    pub const fn view_scale(&self) -> f64 {
        self.view_scale
    }

    /// Records the document-to-screen scale the view is currently drawing at.
    ///
    /// Pure view bookkeeping: it changes no pixel of the composite, so it
    /// bumps nothing. Ignores non-finite and non-positive values rather than
    /// letting a degenerate layout poison every later hit test.
    pub const fn set_view_scale(&mut self, scale: f64) {
        if scale.is_finite() && scale > 0.0 {
            self.view_scale = scale;
        }
    }

    /// Whether undo is available.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    /// How many undo steps are available.
    ///
    /// Exposed because "did that gesture record a step?" is otherwise
    /// unobservable, and the difference between recording one and recording none
    /// is exactly what separates a real edit from an abandoned one.
    #[must_use]
    pub fn undo_depth(&self) -> usize {
        self.history.undo_depth()
    }

    /// How many redo steps are available.
    #[must_use]
    pub fn redo_depth(&self) -> usize {
        self.history.redo_depth()
    }

    /// Whether there is anything to redo.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Whether the dedicated crop mode is active.
    #[must_use]
    pub const fn crop_mode(&self) -> bool {
        self.crop.is_some()
    }

    /// The crop rectangle being previewed without mutating the document.
    #[must_use]
    pub fn pending_crop(&self) -> Option<LogicalRect> {
        self.crop.map(|crop| crop.rect)
    }

    /// Current crop aspect preset.
    #[must_use]
    pub fn crop_aspect(&self) -> Option<CropAspect> {
        self.crop.map(|crop| crop.aspect)
    }

    /// Changes the crop aspect around its current centre.
    pub fn set_crop_aspect(&mut self, aspect: CropAspect) {
        let Some(mut crop) = self.crop else {
            return;
        };
        crop.aspect = aspect;
        if let Some(ratio) = aspect.ratio(self.document.logical_size()) {
            crop.rect = crop_rect_with_size(
                crop.rect,
                crop.rect.size.width,
                crop.rect.size.width / ratio,
                self.document.logical_bounds(),
                Some(ratio),
            );
        }
        if self.crop != Some(crop) {
            self.crop = Some(crop);
            self.touch_view();
        }
    }

    /// Whether crop edges snap to the source boundary.
    #[must_use]
    pub fn crop_snap_edges(&self) -> bool {
        self.crop.is_some_and(|crop| crop.snap_edges)
    }

    /// Enables or disables crop-edge snapping.
    pub fn set_crop_snap_edges(&mut self, enabled: bool) {
        if let Some(crop) = self.crop.as_mut()
            && crop.snap_edges != enabled
        {
            crop.snap_edges = enabled;
            self.touch_view();
        }
    }

    /// Sets the draft crop width in source logical points.
    pub fn set_crop_width(&mut self, width: f64) {
        let Some(mut crop) = self.crop else {
            return;
        };
        let ratio = crop.aspect.ratio(self.document.logical_size());
        let height = ratio.map_or(crop.rect.size.height, |ratio| width / ratio);
        crop.rect = crop_rect_with_size(
            crop.rect,
            width,
            height,
            self.document.logical_bounds(),
            ratio,
        );
        if self.crop != Some(crop) {
            self.crop = Some(crop);
            self.touch_view();
        }
    }

    /// Sets the draft crop height in source logical points.
    pub fn set_crop_height(&mut self, height: f64) {
        let Some(mut crop) = self.crop else {
            return;
        };
        let ratio = crop.aspect.ratio(self.document.logical_size());
        let width = ratio.map_or(crop.rect.size.width, |ratio| height * ratio);
        crop.rect = crop_rect_with_size(
            crop.rect,
            width,
            height,
            self.document.logical_bounds(),
            ratio,
        );
        if self.crop != Some(crop) {
            self.crop = Some(crop);
            self.touch_view();
        }
    }

    /// Swaps draft width and height, including oriented aspect presets.
    pub fn swap_crop_dimensions(&mut self) {
        let Some(mut crop) = self.crop else {
            return;
        };
        crop.aspect = crop.aspect.swapped();
        let ratio = crop.aspect.ratio(self.document.logical_size());
        crop.rect = crop_rect_with_size(
            crop.rect,
            crop.rect.size.height,
            crop.rect.size.width,
            self.document.logical_bounds(),
            ratio,
        );
        self.crop = Some(crop);
        self.touch_view();
    }

    /// Draft and source image dimensions in physical pixels.
    #[must_use]
    pub fn crop_pixel_sizes(&self) -> Option<((u32, u32), (u32, u32))> {
        let crop = self.crop?;
        let scale = self.document.source.frame.scale.get();
        let quantise = |value: f64| {
            value
                .mul_add(scale, 0.0)
                .round()
                .clamp(1.0, f64::from(u32::MAX)) as u32
        };
        Some((
            (
                quantise(crop.rect.size.width),
                quantise(crop.rect.size.height),
            ),
            (
                self.document.source.frame.width(),
                self.document.source.frame.height(),
            ),
        ))
    }

    /// Whether a pointer gesture is in progress.
    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.drag != Drag::Idle
    }

    /// Whether the active gesture is viewport panning.
    #[must_use]
    pub fn is_panning(&self) -> bool {
        matches!(self.drag, Drag::Panning { .. })
    }

    /// Whether the distinct arrow bend affordance is being dragged.
    #[must_use]
    pub fn is_dragging_arrow_bend(&self) -> bool {
        matches!(self.drag, Drag::ArrowBend { .. })
    }

    // -----------------------------------------------------------------------
    // Pointer
    // -----------------------------------------------------------------------

    /// Whether a press at `point` would start editing a label — by placing a
    /// new one, or by re-entering one that is already in the document.
    ///
    /// This is the single place the rule about not opening a second label while
    /// one is still pending is decided, so it has to agree with every route to
    /// [`begin_text`](Self::begin_text): the text tool, and a select click that
    /// lands on a label rather than on a resize handle. A route added without a
    /// matching arm here would be caught by the assertion in `begin_text`.
    fn press_opens_a_label(&self, point: LogicalPoint) -> bool {
        match self.tool {
            Tool::Text => true,
            Tool::Select => {
                // A press on a resize handle never reaches the hit test.
                if self.handle_at(point).is_some() && self.selection_bounds().is_some() {
                    return false;
                }
                self.document
                    .hit_test(point)
                    .and_then(|id| self.document.get(id))
                    .is_some_and(|object| matches!(object.annotation, Annotation::Text { .. }))
            }
            _ => false,
        }
    }

    /// Handles a primary-button press at `point`, in document coordinates.
    ///
    /// Settles an unfinished label first, so that a drawing never lands on top
    /// of one still waiting to be dealt with. When that is refused, a press that
    /// would open *another* label does nothing at all — see
    /// [`press_opens_a_label`](Self::press_opens_a_label). Anything else carries
    /// on: see [`settle_text`](Self::settle_text) for why that is safe.
    pub fn pointer_pressed(&mut self, point: LogicalPoint) {
        if !self.settle_text() && self.press_opens_a_label(point) {
            return;
        }
        if self.tool == Tool::Arrow && self.press_arrow(point) {
            return;
        }
        let changed = match self.tool {
            Tool::Select => {
                self.press_select(point);
                false
            }
            Tool::Crop => {
                if let Some(crop) = self.crop {
                    let action = self
                        .crop_handle_at(point)
                        .map(CropDrag::Resize)
                        .or_else(|| geom::contains(&crop.rect, point).then_some(CropDrag::Move));
                    if let Some(action) = action {
                        self.drag = Drag::CropAdjust {
                            action,
                            grab: point,
                            start: crop.rect,
                        };
                    }
                }
                false
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
                true
            }
            tool if tool.is_click_place() => {
                self.place(tool, point);
                true
            }
            _ => {
                self.drag = Drag::Creating {
                    origin: point,
                    id: None,
                };
                false
            }
        };
        if changed {
            self.touch();
        }
    }

    /// Handles the pointer moving to `point` while the button is down.
    ///
    /// `constrain` is the shift key: it snaps free endpoints to an axis or the
    /// diagonal, and locks a move to one axis.
    pub fn pointer_dragged(&mut self, point: LogicalPoint, constrain: bool) {
        self.pointer_dragged_with_snap(point, constrain, false);
    }

    /// Handles a pointer drag with Command/Ctrl temporarily disabling crop snap.
    pub fn pointer_dragged_with_snap(
        &mut self,
        point: LogicalPoint,
        constrain: bool,
        disable_crop_snap: bool,
    ) {
        let mut changed = false;
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
                let (id, grew) = self.grow(origin, end, id);
                changed = grew;
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
                    changed = true;
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
                    if self.selection_bounds() != Some(moved) {
                        self.document.set_bounds(id, moved);
                        changed = true;
                    }
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
                    let resized = enforce_min(resized);
                    if self.selection_bounds() != Some(resized) {
                        self.document.set_bounds(id, resized);
                        changed = true;
                    }
                }
                self.drag = Drag::Resizing {
                    handle,
                    grab,
                    start,
                };
            }
            Drag::ArrowEndpoint { handle, from, to } => {
                let fixed = match handle {
                    Handle::ArrowStart => to,
                    Handle::ArrowEnd => from,
                    _ => unreachable!("only arrow endpoints enter this drag"),
                };
                let point = if constrain {
                    constrain_to_axis(fixed, point)
                } else {
                    point
                };
                if let Some(id) = self.selection
                    && let Some(mut object) = self.document.get_mut(id)
                    && let Annotation::Arrow {
                        from: current_from,
                        to: current_to,
                    } = object.annotation()
                {
                    let target = match handle {
                        Handle::ArrowStart => current_from,
                        Handle::ArrowEnd => current_to,
                        _ => unreachable!("only arrow endpoints enter this drag"),
                    };
                    if *target != point && geom::distance(fixed, point) >= MIN_DRAG {
                        *target = point;
                        changed = true;
                    }
                }
                self.drag = Drag::ArrowEndpoint { handle, from, to };
            }
            Drag::ArrowBend { from, to, bend } => {
                let midpoint = LogicalPoint::new((from.x + to.x) * 0.5, (from.y + to.y) * 0.5);
                let dx = to.x - from.x;
                let dy = to.y - from.y;
                let length = dx.hypot(dy);
                if length > f64::EPSILON {
                    let signed =
                        ((point.x - midpoint.x) * -dy + (point.y - midpoint.y) * dx) / length;
                    let next = (signed * 2.0 / length).clamp(-0.75, 0.75);
                    if let Some(id) = self.selection
                        && let Some(mut object) = self.document.get_mut(id)
                        && (object.style().effective_arrow_bend() - next).abs() > f64::EPSILON
                    {
                        object.style().arrow_bend = next;
                        self.style.arrow_bend = next;
                        changed = true;
                    }
                }
                self.drag = Drag::ArrowBend { from, to, bend };
            }
            Drag::CropAdjust {
                action,
                grab,
                start,
            } => {
                self.update_crop_drag(action, grab, start, point, constrain, disable_crop_snap);
                self.drag = Drag::CropAdjust {
                    action,
                    grab,
                    start,
                };
            }
            Drag::Panning { grab, start } => {
                // The pan itself arrives through `pan_to`, which is view-only.
                // Restoring the state and returning keeps it that way even if a
                // host feeds pan gestures through the ordinary drag path.
                self.drag = Drag::Panning { grab, start };
                self.touch_view();
                return;
            }
        }
        if changed {
            self.touch();
        }
    }

    /// Handles the primary button being released.
    pub fn pointer_released(&mut self) {
        let drag = std::mem::replace(&mut self.drag, Drag::Idle);
        // Letting go of a pan changes nothing the renderer has to redraw: the
        // same picture is simply somewhere else. Falling through to the commit
        // and the content touch below would spend a document revision, and the
        // preview would be rasterised and re-uploaded at 2400px for a mouse-up.
        if matches!(drag, Drag::Panning { .. }) {
            self.touch_view();
            return;
        }
        let mut changed = false;
        match drag {
            Drag::Creating { id: Some(id), .. } => {
                self.selection = Some(id);
                // Drawing tools stay in hand: the user is usually drawing more
                // than one arrow, and jumping back to Select after each would
                // make the second one a re-pick every time.
            }
            Drag::Drawing { id, points } => {
                if points.len() < 2 {
                    changed = self.document.remove(id).is_some();
                }
            }
            Drag::CropAdjust { .. } => {}
            _ => {}
        }
        self.commit();
        // Releasing the mouse ends a gesture — except when it opened a label,
        // where the caret is still blinking and the user is only halfway
        // through making one thing. Sealing here would split "add a label" and
        // "type in it" into two undo steps.
        if self.editing_text.is_none() {
            self.history.seal();
        }
        if changed {
            self.touch();
        }
    }

    /// Begins panning from a screen-space grab point.
    pub fn begin_pan(&mut self, at: (f32, f32)) {
        self.drag = Drag::Panning {
            grab: at,
            start: self.pan,
        };
    }

    /// Continues a pan to a screen-space point.
    ///
    /// Through [`set_pan`](Self::set_pan) deliberately: panning changes where
    /// the picture sits, not what it contains, so it must not spend a document
    /// revision and force the 2400px preview to be rasterised and re-uploaded
    /// on every mouse move.
    pub fn pan_to(&mut self, at: (f32, f32)) {
        if let Drag::Panning { grab, start } = self.drag {
            self.set_pan((start.0 + at.0 - grab.0, start.1 + at.1 - grab.1));
        }
    }

    /// Abandons any gesture in progress, undoing what it created.
    pub fn cancel_drag(&mut self) {
        let drag = std::mem::replace(&mut self.drag, Drag::Idle);
        if let Drag::CropAdjust { start, .. } = &drag {
            if let Some(crop) = self.crop.as_mut()
                && crop.rect != *start
            {
                crop.rect = *start;
                self.touch_view();
            }
            self.history.seal();
            return;
        }
        let changed = match drag {
            Drag::Creating { id: Some(id), .. } | Drag::Drawing { id, .. } => {
                let changed = self.document.remove(id).is_some();
                if self.selection == Some(id) {
                    self.selection = None;
                }
                changed
            }
            Drag::Moving { start, .. } => {
                if let Some(id) = self.selection {
                    let changed = self.selection_bounds() != Some(start);
                    self.document.set_bounds(id, start);
                    changed
                } else {
                    false
                }
            }
            Drag::Resizing { start, .. } => {
                if let Some(id) = self.selection {
                    let changed = self.selection_bounds() != Some(start);
                    self.document.set_bounds(id, start);
                    changed
                } else {
                    false
                }
            }
            Drag::ArrowEndpoint { from, to, .. } => {
                if let Some(id) = self.selection
                    && let Some(mut object) = self.document.get_mut(id)
                    && let Annotation::Arrow {
                        from: current_from,
                        to: current_to,
                    } = object.annotation()
                {
                    let changed = *current_from != from || *current_to != to;
                    *current_from = from;
                    *current_to = to;
                    changed
                } else {
                    false
                }
            }
            Drag::ArrowBend { bend, .. } => {
                if let Some(id) = self.selection
                    && let Some(mut object) = self.document.get_mut(id)
                {
                    let changed =
                        (object.style().effective_arrow_bend() - bend).abs() > f64::EPSILON;
                    object.style().arrow_bend = bend;
                    self.style.arrow_bend = bend;
                    changed
                } else {
                    false
                }
            }
            _ => false,
        };
        self.history.seal();
        if changed {
            self.touch();
        }
    }

    /// The handle under `point`, if the selection has one there.
    ///
    /// [`HANDLE_TOLERANCE`] is a *screen* distance, because it describes how
    /// close a pointer has to get to a handle drawn at a fixed on-screen size.
    /// `point` is in document coordinates, so the tolerance is divided by the
    /// current view scale. Without that division the grab target shrinks as you
    /// zoom in — at 8x a handle drawn 9 px across would only accept a click
    /// within 1 px of its centre — and balloons as you zoom out, where a
    /// fit-to-window view of a 5K capture would make every handle swallow
    /// clicks a hundred points away.
    #[must_use]
    pub fn handle_at(&self, point: LogicalPoint) -> Option<Handle> {
        let tolerance = self.handle_tolerance();
        self.selection_handles()
            .into_iter()
            .map(|(handle, position)| (handle, geom::distance(position, point)))
            .filter(|(_, distance)| *distance <= tolerance)
            .min_by(|(_, left), (_, right)| left.total_cmp(right))
            .map(|(handle, _)| handle)
    }

    /// The crop edge or corner under `point`, using a screen-sized target.
    #[must_use]
    pub fn crop_handle_at(&self, point: LogicalPoint) -> Option<Handle> {
        let crop = self.crop?;
        let tolerance = self.handle_tolerance();
        Handle::ALL
            .into_iter()
            .map(|handle| (handle, geom::distance(handle.position(&crop.rect), point)))
            .filter(|(_, distance)| *distance <= tolerance)
            .min_by(|(_, left), (_, right)| left.total_cmp(right))
            .map(|(handle, _)| handle)
    }

    fn update_crop_drag(
        &mut self,
        action: CropDrag,
        grab: LogicalPoint,
        start: LogicalRect,
        point: LogicalPoint,
        constrain: bool,
        disable_snap: bool,
    ) {
        let Some(crop) = self.crop else {
            return;
        };
        let bounds = self.document.logical_bounds();
        let snap = crop.snap_edges && !disable_snap;
        let tolerance = CROP_SNAP_TOLERANCE / self.view_scale;
        let ratio = if constrain && crop.aspect == CropAspect::Freeform {
            Some(1.0)
        } else {
            crop.aspect.ratio(self.document.logical_size())
        };
        let next = match action {
            CropDrag::Move => move_crop(
                start,
                point.x - grab.x,
                point.y - grab.y,
                bounds,
                snap,
                tolerance,
            ),
            CropDrag::Resize(handle) => {
                let point = if snap {
                    snap_resize_point(point, handle, bounds, tolerance)
                } else {
                    point
                };
                resize_crop(start, handle, grab, point, bounds, ratio)
            }
        };
        if next != crop.rect {
            self.crop = Some(CropSession { rect: next, ..crop });
            self.touch_view();
        }
    }

    /// [`HANDLE_TOLERANCE`] expressed in document points at the current zoom.
    #[must_use]
    pub fn handle_tolerance(&self) -> f64 {
        HANDLE_TOLERANCE / self.view_scale
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    /// Runs a command, reporting what the host should do next.
    ///
    /// Only commands that change rendered document pixels bump the content
    /// revision. Tool, selection and viewport commands update editor chrome but
    /// leave the cached composite and prepared drag bytes valid.
    ///
    /// Commands that touch the document settle an unfinished label first — see
    /// [`Command::settles_unfinished_text`].
    ///
    /// # Errors
    ///
    /// Returns an error if an undo or redo could not be applied. The state is
    /// left usable either way.
    pub fn command(&mut self, command: Command) -> Result<Intent> {
        if command.settles_unfinished_text() {
            // A refusal is safe to carry on from: whatever this command commits
            // lands above a rollback point the history has already ruled out,
            // so the cancellation stays refused rather than reaching back
            // underneath it.
            let _ = self.settle_text();
        }
        let intent = match command {
            Command::Escape => self.escape(),
            Command::Delete => {
                if let Some(id) = self.selection.take() {
                    let changed = self.document.remove(id).is_some();
                    self.editing_text = None;
                    self.commit();
                    if changed {
                        self.touch();
                    }
                } else if self.document.crop().is_some() {
                    // With nothing selected, delete clears the crop: it is the
                    // only other thing on screen that can be "removed".
                    let _ = self.document.set_crop(None);
                    self.commit();
                    self.touch();
                }
                Intent::None
            }
            Command::Undo => {
                self.cancel_drag();
                // Only when the document actually moved. An undo with nothing
                // behind it is not an edit, and treating it as one would clear
                // a composition in flight and tell the platform IME to cancel
                // itself over a keystroke that did nothing.
                if self.history.undo(&mut self.document)? {
                    self.after_history();
                    self.touch();
                }
                Intent::None
            }
            Command::Redo => {
                self.cancel_drag();
                if self.history.redo(&mut self.document)? {
                    self.after_history();
                    self.touch();
                }
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
                if let Some(crop) = self.crop {
                    let next = move_crop(
                        crop.rect,
                        dx,
                        dy,
                        self.document.logical_bounds(),
                        crop.snap_edges,
                        CROP_SNAP_TOLERANCE / self.view_scale,
                    );
                    if next != crop.rect {
                        self.crop = Some(CropSession { rect: next, ..crop });
                        self.touch_view();
                    }
                } else if let Some(id) = self.selection
                    && (dx != 0.0 || dy != 0.0)
                    && self.document.translate(id, dx, dy)
                {
                    self.commit_coalesced("nudge");
                    self.touch();
                }
                Intent::None
            }
            Command::ZoomIn => {
                self.zoom_about(self.effective_zoom() * ZOOM_STEP, (0.0, 0.0));
                Intent::None
            }
            Command::ZoomOut => {
                self.zoom_about(self.effective_zoom() / ZOOM_STEP, (0.0, 0.0));
                Intent::None
            }
            Command::ZoomReset => {
                if !self.fit_zoom || self.pan != (0.0, 0.0) {
                    self.fit_zoom = true;
                    self.zoom = 1.0;
                    self.pan = (0.0, 0.0);
                    self.touch_view();
                }
                Intent::None
            }
            Command::ApplyCrop => {
                if let Some(crop) = self.crop.take() {
                    self.drag = Drag::Idle;
                    let before = self.document.crop();
                    let _ = self.document.set_crop(Some(crop.rect));
                    self.tool = Tool::Select;
                    if self.document.crop() != before {
                        self.commit();
                        self.touch();
                    }
                }
                Intent::None
            }
            Command::CancelCrop => {
                if self.crop.take().is_some() {
                    self.drag = Drag::Idle;
                    self.tool = Tool::Select;
                    self.touch_view();
                }
                Intent::None
            }
            Command::RevertCrop => {
                self.crop = None;
                self.drag = Drag::Idle;
                self.tool = Tool::Select;
                if self.document.crop().is_some() {
                    let _ = self.document.set_crop(None);
                    self.commit();
                    self.touch();
                } else {
                    self.touch_view();
                }
                Intent::None
            }
            Command::SendToBack => {
                if let Some(id) = self.selection {
                    let changed = self.document.z_index(id).is_some_and(|index| index != 0);
                    if changed && self.document.send_to_back(id) {
                        self.commit();
                        self.touch();
                    }
                }
                Intent::None
            }
            Command::BringToFront => {
                if let Some(id) = self.selection {
                    let top = self.document.len().saturating_sub(1);
                    let changed = self.document.z_index(id).is_some_and(|index| index != top);
                    if changed && self.document.bring_to_front(id) {
                        self.commit();
                        self.touch();
                    }
                }
                Intent::None
            }
        };
        Ok(intent)
    }

    /// Escape unwinds one layer of state at a time, closing only once there is
    /// nothing left to back out of. Closing straight away would throw away a
    /// half-drawn shape *and* the window, when the user meant only the shape.
    fn escape(&mut self) -> Intent {
        // The set-aside case counts as editing: the label is unfinished even
        // though an undo has taken it out of the document, and Escape is how the
        // user says they are done with it.
        if (self.editing_text.is_some() || self.suspended_text.is_some()) && self.settle_text() {
            return Intent::None;
        }
        if self.is_dragging() {
            self.cancel_drag();
        } else if self.crop.take().is_some() {
            self.tool = Tool::Select;
            self.touch_view();
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

    fn press_arrow(&mut self, point: LogicalPoint) -> bool {
        if let Some(handle @ (Handle::ArrowStart | Handle::ArrowEnd)) = self.handle_at(point)
            && let Some(object) = self.selection.and_then(|id| self.document.get(id))
            && let Annotation::Arrow { from, to } = &object.annotation
        {
            self.drag = Drag::ArrowEndpoint {
                handle,
                from: *from,
                to: *to,
            };
            return true;
        }
        if self.press_arrow_bend(point) {
            return true;
        }

        let hit = self.document.hit_test_all(point).into_iter().find(|id| {
            self.document
                .get(*id)
                .is_some_and(|object| matches!(object.annotation, Annotation::Arrow { .. }))
        });
        let Some(id) = hit else {
            self.selection = None;
            return false;
        };
        self.selection = Some(id);
        if let Some(object) = self.document.get(id) {
            self.style = object.style;
            self.drag = Drag::Moving {
                grab: point,
                start: object.bounds(),
            };
        }
        true
    }

    fn press_select(&mut self, point: LogicalPoint) {
        if let Some(handle) = self.handle_at(point) {
            if matches!(handle, Handle::ArrowStart | Handle::ArrowEnd)
                && let Some(object) = self.selection.and_then(|id| self.document.get(id))
                && let Annotation::Arrow { from, to } = &object.annotation
            {
                self.drag = Drag::ArrowEndpoint {
                    handle,
                    from: *from,
                    to: *to,
                };
                return;
            }
            if let Some(start) = self.selection_bounds() {
                self.drag = Drag::Resizing {
                    handle,
                    grab: point,
                    start,
                };
                return;
            }
        }
        if self.press_arrow_bend(point) {
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
                        // Re-entering a label that already exists: emptying it is
                        // a real deletion, undoable like any other.
                        self.begin_text(id, false);
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

    fn press_arrow_bend(&mut self, point: LogicalPoint) -> bool {
        let Some(handle) = self.arrow_bend_handle() else {
            return false;
        };
        if geom::distance(handle, point) > self.handle_tolerance() {
            return false;
        }
        let Some(object) = self.selection.and_then(|id| self.document.get(id)) else {
            return false;
        };
        let Annotation::Arrow { from, to } = object.annotation else {
            return false;
        };
        self.drag = Drag::ArrowBend {
            from,
            to,
            bend: object.style.effective_arrow_bend(),
        };
        true
    }

    fn place(&mut self, tool: Tool, point: LogicalPoint) {
        let id = match tool {
            Tool::Text => {
                // The rollback point has to predate the annotation, so this
                // must happen before the document is touched: abandoning is
                // "the click never happened", and the click added this.
                self.history.begin();
                let id = self.document.add(
                    Annotation::Text {
                        at: point,
                        content: String::new(),
                    },
                    self.style,
                );
                self.begin_text(id, true);
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
        if tool == Tool::Text {
            // A label and the words in it are one thing to the user. Coalescing
            // the creation into the same entry as the typing means one undo
            // takes the whole label away, instead of leaving an empty box that
            // draws nothing and can still be clicked.
            self.commit_coalesced("text");
        } else {
            self.commit();
        }
    }

    /// Grows the annotation being dragged out, creating it on the first move
    /// that clears [`MIN_DRAG`].
    fn grow(
        &mut self,
        origin: LogicalPoint,
        point: LogicalPoint,
        existing: Option<AnnotationId>,
    ) -> (Option<AnnotationId>, bool) {
        if existing.is_none() && geom::distance(origin, point) < MIN_DRAG {
            return (None, false);
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
            _ => return (existing, false),
        };
        match existing {
            Some(id) => {
                let changed = self
                    .document
                    .get(id)
                    .is_some_and(|object| object.annotation != annotation);
                if changed && let Some(mut object) = self.document.get_mut(id) {
                    *object.annotation() = annotation;
                }
                (Some(id), changed)
            }
            None => (
                Some(self.document.add(annotation, self.style_for_new())),
                true,
            ),
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
        if let Some(object) = self.selection.and_then(|id| self.document.get(id)) {
            self.style = object.style;
        }
        // Editing follows the annotation. Undoing the click that made a label
        // takes the label out of the document, so editing has to stop — but the
        // undo is redoable, and a redo puts an empty, invisible label back. If
        // nothing remembered which one it was, it would come back with nobody
        // editing it: unreachable by typing, and impossible to cancel, because
        // `finish_text` has no annotation to abandon. So the id is set aside on
        // the way down and picked up again on the way back.
        match self.editing_text {
            Some(id) if self.document.get(id).is_none() => {
                self.suspended_text = Some(id);
                self.editing_text = None;
            }
            Some(_) => {}
            None => {
                if let Some(id) = self.suspended_text
                    && self.document.get(id).is_some()
                {
                    self.editing_text = Some(id);
                    self.suspended_text = None;
                }
            }
        }

        // A composition belongs to the keystrokes that were just undone. Keeping
        // its byte range would leave `text_edit` addressing text that no longer
        // exists, and the platform IME has no idea the document moved under it.
        self.preedit = None;
        self.interrupt_ime = true;

        // The caret goes back to where it was when this state was last on
        // screen, not wherever the user happened to leave it. Typing "a", then
        // "é", then undoing and redoing must leave the caret after the "é" — a
        // clamp alone would leave it at 1 and the next character would land in
        // the middle. The clamp is still applied on top, because a marker
        // recorded against different text can point inside a code point and
        // slicing there would panic.
        let mark = usize::try_from(self.history.current_mark()).unwrap_or(usize::MAX);
        self.caret = match self.editing_text.and_then(|id| self.text_of(id)) {
            Some(text) => clamp_boundary(text, mark),
            None => 0,
        };

        self.dirty = true;
    }

    /// Records the caret against the state the history is about to keep.
    fn mark_caret(&mut self) {
        self.history
            .mark(u64::try_from(self.caret).unwrap_or(u64::MAX));
    }

    /// Takes the pending instruction to interrupt the platform's composition.
    ///
    /// One-shot: the flag is cleared by reading it, because the instruction has
    /// to reach the IME exactly once. Leaving it set would cancel the next
    /// composition the user starts, which is indistinguishable from the IME
    /// being broken.
    pub const fn take_ime_interrupt(&mut self) -> bool {
        let pending = self.interrupt_ime;
        self.interrupt_ime = false;
        pending
    }

    /// Whether an interruption is still waiting to be sent.
    #[must_use]
    pub const fn ime_interrupt_pending(&self) -> bool {
        self.interrupt_ime
    }

    /// The content of `id`, if it is a text annotation.
    fn text_of(&self, id: AnnotationId) -> Option<&str> {
        match &self.document.get(id)?.annotation {
            Annotation::Text { content, .. } => Some(content.as_str()),
            _ => None,
        }
    }

    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    const fn touch_view(&mut self) {
        self.view_revision = self.view_revision.wrapping_add(1);
    }
}

/// The nearest `char` boundary at or before `at`, never past the end.
fn clamp_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The `char` boundary before `at`, or `None` at the start of the string.
fn prev_boundary(text: &str, at: usize) -> Option<usize> {
    text.get(..at)?.char_indices().next_back().map(|(i, _)| i)
}

/// The `char` boundary after `at`, or `None` at the end of the string.
fn next_boundary(text: &str, at: usize) -> Option<usize> {
    let rest = text.get(at..)?;
    rest.chars().next().map(|ch| at + ch.len_utf8())
}

/// Adjusts a composition range for a deletion of `start..end`.
///
/// A composition that overlapped the deleted span is abandoned rather than
/// truncated: half a half-typed glyph is not a composition anyone can finish.
fn shift_preedit(preedit: Option<Range<usize>>, start: usize, end: usize) -> Option<Range<usize>> {
    let range = preedit?;
    if range.start >= end {
        let by = end - start;
        Some(range.start - by..range.end - by)
    } else if range.end <= start {
        Some(range)
    } else {
        None
    }
}

/// Where a caret motion lands, as a byte offset on a `char` boundary.
fn move_caret(text: &str, caret: usize, motion: Caret) -> usize {
    let line_start = text[..caret].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[caret..].find('\n').map_or(text.len(), |i| caret + i);
    match motion {
        Caret::Left => prev_boundary(text, caret).unwrap_or(0),
        Caret::Right => next_boundary(text, caret).unwrap_or(caret),
        Caret::LineStart => line_start,
        Caret::LineEnd => line_end,
        Caret::Up | Caret::Down => {
            let column = text[line_start..caret].chars().count();
            let target = if motion == Caret::Up {
                if line_start == 0 {
                    return 0;
                }
                let above_end = line_start - 1;
                let above_start = text[..above_end].rfind('\n').map_or(0, |i| i + 1);
                above_start..above_end
            } else {
                if line_end == text.len() {
                    return text.len();
                }
                let below_start = line_end + 1;
                let below_end = text[below_start..]
                    .find('\n')
                    .map_or(text.len(), |i| below_start + i);
                below_start..below_end
            };
            // Clamp to the shorter line rather than overshooting into the next.
            let line = &text[target.clone()];
            line.char_indices()
                .nth(column)
                .map_or(target.end, |(i, _)| target.start + i)
        }
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
        AnnotationKind::Highlight => {
            let recolor = |current: Color| {
                Color::rgba(style.stroke.r, style.stroke.g, style.stroke.b, current.a)
            };
            object
                .style
                .with_stroke(recolor(object.style.stroke))
                .with_fill(object.style.fill.map(recolor))
        }
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

fn crop_rect_with_size(
    current: LogicalRect,
    width: f64,
    height: f64,
    bounds: LogicalRect,
    ratio: Option<f64>,
) -> LogicalRect {
    let fallback = current.size;
    let (width, height) = fit_crop_size(width, height, fallback, bounds.size, ratio);
    let center = geom::center(&current);
    clamp_crop_origin(
        LogicalRect::new(
            LogicalPoint::new(center.x - width / 2.0, center.y - height / 2.0),
            LogicalSize::new(width, height),
        ),
        bounds,
    )
}

fn move_crop(
    start: LogicalRect,
    dx: f64,
    dy: f64,
    bounds: LogicalRect,
    snap: bool,
    tolerance: f64,
) -> LogicalRect {
    let mut rect = LogicalRect::new(
        LogicalPoint::new(start.origin.x + dx, start.origin.y + dy),
        start.size,
    );
    if snap {
        if (rect.origin.x - bounds.origin.x).abs() <= tolerance {
            rect.origin.x = bounds.origin.x;
        } else if (geom::max_x(&bounds) - geom::max_x(&rect)).abs() <= tolerance {
            rect.origin.x = geom::max_x(&bounds) - rect.size.width;
        }
        if (rect.origin.y - bounds.origin.y).abs() <= tolerance {
            rect.origin.y = bounds.origin.y;
        } else if (geom::max_y(&bounds) - geom::max_y(&rect)).abs() <= tolerance {
            rect.origin.y = geom::max_y(&bounds) - rect.size.height;
        }
    }
    clamp_crop_origin(rect, bounds)
}

fn resize_crop(
    start: LogicalRect,
    handle: Handle,
    grab: LogicalPoint,
    point: LogicalPoint,
    bounds: LogicalRect,
    ratio: Option<f64>,
) -> LogicalRect {
    let raw = handle.resize(&start, point.x - grab.x, point.y - grab.y);
    let (proposed_width, proposed_height) = match ratio {
        Some(ratio) if matches!(handle, Handle::Left | Handle::Right) => {
            (raw.size.width, raw.size.width / ratio)
        }
        Some(ratio) if matches!(handle, Handle::Top | Handle::Bottom) => {
            (raw.size.height * ratio, raw.size.height)
        }
        Some(ratio) if raw.size.width / raw.size.height > ratio => {
            (raw.size.width, raw.size.width / ratio)
        }
        Some(ratio) => (raw.size.height * ratio, raw.size.height),
        None => (raw.size.width, raw.size.height),
    };
    let (width, height) = fit_crop_size(
        proposed_width,
        proposed_height,
        start.size,
        bounds.size,
        ratio,
    );
    let center = geom::center(&start);
    let x = if handle.moves_left() {
        geom::max_x(&start) - width
    } else if handle.moves_right() {
        start.origin.x
    } else {
        center.x - width / 2.0
    };
    let y = if handle.moves_top() {
        geom::max_y(&start) - height
    } else if handle.moves_bottom() {
        start.origin.y
    } else {
        center.y - height / 2.0
    };
    clamp_crop_origin(
        LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(width, height)),
        bounds,
    )
}

fn snap_resize_point(
    mut point: LogicalPoint,
    handle: Handle,
    bounds: LogicalRect,
    tolerance: f64,
) -> LogicalPoint {
    if handle.moves_left() || handle.moves_right() {
        if (point.x - bounds.origin.x).abs() <= tolerance {
            point.x = bounds.origin.x;
        } else if (point.x - geom::max_x(&bounds)).abs() <= tolerance {
            point.x = geom::max_x(&bounds);
        }
    }
    if handle.moves_top() || handle.moves_bottom() {
        if (point.y - bounds.origin.y).abs() <= tolerance {
            point.y = bounds.origin.y;
        } else if (point.y - geom::max_y(&bounds)).abs() <= tolerance {
            point.y = geom::max_y(&bounds);
        }
    }
    point
}

fn fit_crop_size(
    width: f64,
    height: f64,
    fallback: LogicalSize,
    bounds: LogicalSize,
    ratio: Option<f64>,
) -> (f64, f64) {
    let mut width = if width.is_finite() {
        width.abs()
    } else {
        fallback.width
    };
    let mut height = if height.is_finite() {
        height.abs()
    } else {
        fallback.height
    };
    if let Some(ratio) = ratio.filter(|ratio| ratio.is_finite() && *ratio > 0.0) {
        height = width / ratio;
        let minimum_scale = (MIN_SIZE / width.max(f64::EPSILON))
            .max(MIN_SIZE / height.max(f64::EPSILON))
            .max(1.0);
        width *= minimum_scale;
        height *= minimum_scale;
        let maximum_scale = (bounds.width / width).min(bounds.height / height).min(1.0);
        width *= maximum_scale;
        height *= maximum_scale;
    } else {
        width = width.clamp(MIN_SIZE.min(bounds.width), bounds.width);
        height = height.clamp(MIN_SIZE.min(bounds.height), bounds.height);
    }
    (width, height)
}

fn clamp_crop_origin(mut rect: LogicalRect, bounds: LogicalRect) -> LogicalRect {
    rect.origin.x = rect.origin.x.clamp(
        bounds.origin.x,
        (geom::max_x(&bounds) - rect.size.width).max(bounds.origin.x),
    );
    rect.origin.y = rect.origin.y.clamp(
        bounds.origin.y,
        (geom::max_y(&bounds) - rect.size.height).max(bounds.origin.y),
    );
    rect
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
