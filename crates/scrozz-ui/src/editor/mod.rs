//! The annotation editor.
//!
//! A capture opens here from its floating card, and this is where arrows,
//! shapes, text, highlights, redactions, step numbers and crops get put on it.
//!
//! # Shape
//!
//! Split the way [`select`](crate::select) is, and for the same reason: the
//! part that decides is testable without a window, and the part that draws is
//! not.
//!
//! * [`state`] — the pure state machine. Tools, selection, drags, keyboard
//!   commands, undo and redo. Works entirely in document logical points.
//! * [`paint`] — the canvas: the rendered preview, transient drag chrome, and
//!   the selection handles.
//! * [`toolbar`] — the tool palette and the colour and stroke inspector.
//! * [`scene`] — the harness [`Scene`](crate::harness::Scene) that draws the
//!   editor for a golden.
//!
//! # Why the preview is a render and not egui drawing
//!
//! The canvas shows a texture produced by [`SkiaRenderer`], the *same*
//! renderer the export uses, cached against
//! [`EditorState::revision`](state::EditorState::revision). It would be far
//! easier to draw the annotations with egui's painter, and it would be wrong:
//! egui cannot blur or pixelate, so a redaction — the one annotation where
//! being wrong actually matters — would preview as something it is not. Going
//! through the renderer means what the user approves is what leaves the app.

#![allow(missing_docs)]

pub mod paint;
pub mod scene;
pub mod state;
pub mod toolbar;

use egui::{Key, Modifiers, Ui};
use scrozz_annotate::Document;
use scrozz_core::{LogicalPoint, LogicalRect};

pub use paint::{CanvasView, Preview, to_color_image};
pub use scene::EditorScene;
pub use state::{
    Caret, Command, EditorState, Handle, Intent, MAX_ZOOM, MIN_DRAG, MIN_SIZE, MIN_ZOOM, NUDGE,
    NUDGE_COARSE, TextEdit, Tool, ZOOM_STEP,
};
pub use toolbar::{PALETTE, STROKE_MAX, STROKE_MIN};

/// The smallest the editor window may be dragged, in points.
///
/// The width is whatever the toolbar's wrapped layout needs, so the window can
/// never be made narrower than its own controls.
pub const MIN_WINDOW_SIZE: [f32; 2] = [
    if toolbar::WRAPPED_W > 560.0 {
        toolbar::WRAPPED_W
    } else {
        560.0
    },
    400.0,
];

/// The editor surface: state, cached preview, and the frame's decisions.
pub struct EditorUi {
    state: EditorState,
    preview: Preview,
    /// A decision reached while painting, returned at the end of the frame.
    ///
    /// Toolbar clicks are handled where they are drawn, but the frame's result
    /// is a single value; parking it here keeps `update` linear instead of
    /// threading a return through every painter.
    pending: Option<Intent>,
}

impl EditorUi {
    /// Opens the editor over a document.
    #[must_use]
    pub fn new(document: Document) -> Self {
        Self {
            state: EditorState::new(document),
            preview: Preview::default(),
            pending: None,
        }
    }

    /// The editor's state.
    #[must_use]
    pub const fn state(&self) -> &EditorState {
        &self.state
    }

    /// The editor's state, mutably.
    pub const fn state_mut(&mut self) -> &mut EditorState {
        &mut self.state
    }

    /// The document being edited.
    #[must_use]
    pub const fn document(&self) -> &Document {
        self.state.document()
    }

    /// Flattens the document to a full-resolution image.
    ///
    /// This is the *only* place an annotated image is produced, so copy, save
    /// and any future export cannot drift apart. It renders from the document
    /// at the capture's own scale — never from the on-screen preview texture,
    /// which is capped at a couple of thousand pixels and would silently
    /// downscale a 6K screenshot on its way to the clipboard.
    ///
    /// # Errors
    ///
    /// Propagates whatever the renderer refuses, which per decision D9
    /// includes compositing a window capture.
    pub fn render(&self) -> scrozz_core::Result<scrozz_core::Frame> {
        use scrozz_annotate::Renderer as _;
        scrozz_annotate::SkiaRenderer.render(self.document())
    }

    /// Draws one frame and reports what the host should do.
    pub fn update(&mut self, ui: &mut Ui) -> Intent {
        let theme = theme_for(ui);
        let icons = shared_icons(ui.ctx());
        let surface = crate::paint::Surface::new(&theme, &icons, crate::motion::Motion::at(0.0));

        if let Some(intent) = self.keyboard(ui) {
            self.pending = Some(intent);
        }

        let full = ui.available_rect_before_wrap();
        // The toolbar wraps rather than overlapping itself, so how much room it
        // needs depends on how wide the window is.
        let toolbar_h = toolbar::height_for(full.width());
        let bar = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), toolbar_h));
        let canvas =
            egui::Rect::from_min_max(egui::pos2(full.left(), full.top() + toolbar_h), full.max);

        let view = paint::draw_canvas(ui, &surface, &mut self.state, &mut self.preview, canvas);
        if let Some(action) = toolbar::draw(ui, &surface, &mut self.state, bar, &view) {
            self.pending = Some(action);
        }

        self.pending.take().unwrap_or(Intent::None)
    }

    /// Maps this frame's input onto editor commands and text edits.
    fn keyboard(&mut self, ui: &Ui) -> Option<Intent> {
        // Typing into a text annotation swallows the accelerators, otherwise
        // pressing "r" mid-word would swap the tool.
        let typing = self.state.editing_text().is_some();
        let mut result = None;
        // One ordered pass: a frame can carry both a keystroke and the text it
        // produced, and applying them out of order would type backwards.
        for input in collect_input(ui, typing) {
            match input {
                Input::Command(command) => match self.state.command(command) {
                    Ok(Intent::None) => {}
                    Ok(intent) => result = Some(intent),
                    Err(error) => tracing::warn!(%error, "editor command failed"),
                },
                Input::Text(edit) => self.state.text_edit(&edit),
            }
        }
        result
    }
}

/// The icon store for a context, built once and kept alive in egui's memory.
///
/// Rasterising every icon costs milliseconds, so it must not happen inside a
/// frame. It also must not happen *per* frame for a subtler reason: a
/// [`TextureHandle`](egui::TextureHandle) frees its texture when dropped, so a
/// store built and dropped within one pass uploads and deletes in the same
/// texture delta and paints nothing at all.
fn shared_icons(ctx: &egui::Context) -> std::sync::Arc<crate::icons::IconStore> {
    let id = egui::Id::new("scrozz-editor-icons");
    // Two steps on purpose: `IconStore::new` uploads textures, and uploading
    // while holding egui's memory lock deadlocks against the texture manager.
    if let Some(existing) = ctx.data(|data| data.get_temp::<Store>(id)) {
        return existing;
    }
    let store: Store = std::sync::Arc::new(crate::icons::IconStore::new(ctx));
    ctx.data_mut(|data| data.insert_temp(id, store.clone()));
    store
}

/// Shorthand for the shared icon store's stored type.
type Store = std::sync::Arc<crate::icons::IconStore>;

/// One thing this frame's input asked for, in the order it arrived.
enum Input {
    /// An accelerator.
    Command(Command),
    /// Text entry, only produced while a text annotation is being typed into.
    Text(TextEdit),
}

/// Reads this frame's input and turns it into commands and text edits.
///
/// Ordering is load-bearing. egui delivers a `Key` event and the `Text` event
/// it produced in the same frame, and a backspace followed by a character is
/// not the same as a character followed by a backspace, so both kinds come back
/// interleaved in arrival order rather than as two separate lists.
fn collect_input(ui: &Ui, typing: bool) -> Vec<Input> {
    ui.ctx().input(|input| {
        let mut out = Vec::new();
        let cmd = input.modifiers.command;
        let shift = input.modifiers.shift;

        for event in &input.events {
            if typing {
                // While typing, text and composition events are input, not
                // accelerators. Command-modified keys still fall through to the
                // accelerator table below so Copy, Save and Undo keep working.
                match event {
                    egui::Event::Text(text) if !cmd => {
                        out.push(Input::Text(TextEdit::Insert(text.clone())));
                        continue;
                    }
                    egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => {
                        out.push(Input::Text(TextEdit::Preedit(text.clone())));
                        continue;
                    }
                    egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                        out.push(Input::Text(TextEdit::Insert(text.clone())));
                        continue;
                    }
                    _ => {}
                }
            }
            let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            else {
                continue;
            };
            let cmd = modifiers.command;
            let shift = modifiers.shift;
            if typing
                && !cmd
                && let Some(edit) = text_key(*key, shift)
            {
                out.push(Input::Text(edit));
                continue;
            }
            match key {
                Key::Escape => out.push(Input::Command(Command::Escape)),
                Key::Z if cmd && shift => out.push(Input::Command(Command::Redo)),
                Key::Z if cmd => out.push(Input::Command(Command::Undo)),
                Key::Y if cmd => out.push(Input::Command(Command::Redo)),
                Key::C if cmd => out.push(Input::Command(Command::Copy)),
                Key::S if cmd => out.push(Input::Command(Command::Save)),
                Key::A if cmd => out.push(Input::Command(Command::SelectAll)),
                Key::Plus | Key::Equals if cmd => out.push(Input::Command(Command::ZoomIn)),
                Key::Minus if cmd => out.push(Input::Command(Command::ZoomOut)),
                Key::Num0 if cmd => out.push(Input::Command(Command::ZoomReset)),
                Key::OpenBracket if cmd => out.push(Input::Command(Command::SendToBack)),
                Key::CloseBracket if cmd => out.push(Input::Command(Command::BringToFront)),
                _ if cmd => {}
                Key::Delete | Key::Backspace if !typing => {
                    out.push(Input::Command(Command::Delete))
                }
                Key::Enter if !typing => out.push(Input::Command(Command::ApplyCrop)),
                // Enter while typing means "done", which is the first layer
                // Escape unwinds, so it commits through exactly the same path.
                Key::Enter if !shift => out.push(Input::Command(Command::Escape)),
                Key::ArrowLeft if !typing => out.push(Input::Command(nudge(-1.0, 0.0, shift))),
                Key::ArrowRight if !typing => out.push(Input::Command(nudge(1.0, 0.0, shift))),
                Key::ArrowUp if !typing => out.push(Input::Command(nudge(0.0, -1.0, shift))),
                Key::ArrowDown if !typing => out.push(Input::Command(nudge(0.0, 1.0, shift))),
                _ => {}
            }
        }

        if !typing && !cmd && !shift {
            for text in input.events.iter().filter_map(|event| match event {
                egui::Event::Text(text) => Some(text),
                _ => None,
            }) {
                if let Some(tool) = text.chars().next().and_then(Tool::from_accelerator) {
                    out.push(Input::Command(Command::Pick(tool)));
                }
            }
        }
        out
    })
}

/// The text edit a bare key performs while typing, if any.
///
/// Enter inserts a newline only with Shift; a plain Enter falls through to the
/// accelerator table, where it means "done", matching every other single-line
/// field the user has ever used.
fn text_key(key: Key, shift: bool) -> Option<TextEdit> {
    let edit = match key {
        Key::Backspace => TextEdit::Backspace,
        Key::Delete => TextEdit::DeleteForward,
        Key::ArrowLeft => TextEdit::Caret(Caret::Left),
        Key::ArrowRight => TextEdit::Caret(Caret::Right),
        Key::ArrowUp => TextEdit::Caret(Caret::Up),
        Key::ArrowDown => TextEdit::Caret(Caret::Down),
        Key::Home => TextEdit::Caret(Caret::LineStart),
        Key::End => TextEdit::Caret(Caret::LineEnd),
        Key::Enter if shift => TextEdit::Insert("\n".to_owned()),
        _ => return None,
    };
    Some(edit)
}

const fn nudge(x: f64, y: f64, coarse: bool) -> Command {
    let step = if coarse {
        state::NUDGE_COARSE
    } else {
        state::NUDGE
    };
    Command::Nudge {
        dx: x * step,
        dy: y * step,
    }
}

fn theme_for(ui: &Ui) -> crate::theme::Theme {
    let appearance = if ui.visuals().dark_mode {
        crate::theme::Appearance::Dark
    } else {
        crate::theme::Appearance::Light
    };
    crate::theme::Theme::for_appearance(appearance)
}

/// A deferred viewport hosting the editor.
///
/// Modelled on [`SettingsWindow`](crate::settings::SettingsWindow) and for the
/// same reason: the capture overlay is a transparent, non-activating panel with
/// mouse passthrough, so an editor drawn inside it could never take keyboard
/// focus. A separate viewport is a real, focusable, resizable window.
#[derive(Debug, Default)]
pub struct EditorWindow {
    open: bool,
    focus_requested: bool,
    title: String,
}

impl EditorWindow {
    /// A closed editor window.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the window is showing.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Opens the window, or raises it if it is already showing.
    pub fn open(&mut self, title: impl Into<String>) {
        self.title = title.into();
        self.open = true;
        self.focus_requested = true;
    }

    /// Closes the window.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Shows the window, calling `build` to draw its contents.
    pub fn show(&mut self, ctx: &egui::Context, build: impl FnMut(&mut Ui)) {
        if !self.open {
            return;
        }
        let focus = std::mem::take(&mut self.focus_requested);
        let title = if self.title.is_empty() {
            "Annotate".to_owned()
        } else {
            format!("Annotate — {}", self.title)
        };
        let mut builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([1040.0, 720.0])
            .with_min_inner_size(MIN_WINDOW_SIZE);
        if focus {
            builder = builder.with_active(true);
        }
        let mut close = false;
        let mut build = build;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("scrozz-editor"),
            builder,
            |editor_ui, _class| {
                if editor_ui
                    .ctx()
                    .input(|input| input.viewport().close_requested())
                {
                    close = true;
                }
                build(editor_ui);
            },
        );
        if close {
            self.open = false;
        }
    }
}

/// The rectangle a document occupies once fitted into `area`.
///
/// Used by the canvas and by tests; free-standing because fitting is pure
/// arithmetic and much easier to check without a `Ui`.
#[must_use]
pub fn fit(content: LogicalRect, area: egui::Rect, zoom: f32, pan: (f32, f32)) -> egui::Rect {
    let (w, h) = (content.size.width as f32, content.size.height as f32);
    if w <= 0.0 || h <= 0.0 || area.width() <= 0.0 || area.height() <= 0.0 {
        return egui::Rect::from_min_size(area.center(), egui::Vec2::ZERO);
    }
    // Never scale a small capture *up* to fill the window: a 200 pt shot blown
    // to 900 pt would look soft and lie about what was captured.
    let scale = (area.width() / w).min(area.height() / h).min(1.0) * zoom;
    let size = egui::vec2(w * scale, h * scale);
    let center = area.center() + egui::vec2(pan.0, pan.1);
    egui::Rect::from_center_size(center, size)
}

/// Converts a screen position into document logical points.
#[must_use]
pub fn to_document(screen: egui::Pos2, canvas: egui::Rect, content: LogicalRect) -> LogicalPoint {
    if canvas.width() <= 0.0 || canvas.height() <= 0.0 {
        return content.origin;
    }
    let fx = f64::from((screen.x - canvas.left()) / canvas.width());
    let fy = f64::from((screen.y - canvas.top()) / canvas.height());
    LogicalPoint::new(
        content.origin.x + fx * content.size.width,
        content.origin.y + fy * content.size.height,
    )
}

/// Converts document logical points into a screen position.
#[must_use]
pub fn to_screen(point: LogicalPoint, canvas: egui::Rect, content: LogicalRect) -> egui::Pos2 {
    if content.size.width <= 0.0 || content.size.height <= 0.0 {
        return canvas.min;
    }
    let fx = (point.x - content.origin.x) / content.size.width;
    let fy = (point.y - content.origin.y) / content.size.height;
    egui::pos2(
        canvas.left() + canvas.width() * fx as f32,
        canvas.top() + canvas.height() * fy as f32,
    )
}

/// Converts a document rectangle into a screen rectangle.
#[must_use]
pub fn rect_to_screen(rect: LogicalRect, canvas: egui::Rect, content: LogicalRect) -> egui::Rect {
    let min = to_screen(rect.origin, canvas, content);
    let max = to_screen(
        LogicalPoint::new(
            rect.origin.x + rect.size.width,
            rect.origin.y + rect.size.height,
        ),
        canvas,
        content,
    );
    egui::Rect::from_min_max(min, max)
}

/// Modifiers, reduced to what the editor cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EditorModifiers {
    /// Shift: constrain a drag to an axis or the diagonal.
    pub constrain: bool,
    /// Space: pan instead of draw.
    pub pan: bool,
}

impl EditorModifiers {
    /// Reads the modifiers from this frame's input.
    #[must_use]
    pub fn read(ui: &Ui) -> Self {
        ui.ctx().input(|input| Self {
            constrain: input.modifiers.shift,
            pan: input.key_down(Key::Space),
        })
    }
}

const _: fn(&Modifiers) = |_| {};
