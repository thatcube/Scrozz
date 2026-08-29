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
use scrozz_core::{Frame, LogicalPoint, LogicalRect};

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

/// A full-resolution document render tied to the content revision it came from.
#[derive(Debug)]
pub struct RevisionedFrame {
    revision: u64,
    frame: Frame,
}

impl RevisionedFrame {
    /// Renders `document` and tags the immutable result with `revision`.
    ///
    /// The caller must take the revision and the document snapshot together.
    /// Rendering accepts only an immutable document, so the pair cannot drift
    /// while the renderer is running.
    pub fn from_document(document: &Document, revision: u64) -> scrozz_core::Result<Self> {
        use scrozz_annotate::Renderer as _;
        let frame = scrozz_annotate::SkiaRenderer.render(document)?;
        Ok(Self { revision, frame })
    }

    /// The document revision that was rendered.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// The flattened image.
    #[must_use]
    pub const fn frame(&self) -> &Frame {
        &self.frame
    }
}

impl std::ops::Deref for RevisionedFrame {
    type Target = Frame;

    fn deref(&self) -> &Self::Target {
        &self.frame
    }
}

/// The editor surface: state, cached preview, and the frame's decisions.
pub struct EditorUi {
    state: EditorState,
    preview: Preview,
    color_popover: toolbar::ColorPopover,
    arrow_popover: toolbar::ArrowPopover,
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
            color_popover: toolbar::ColorPopover::default(),
            arrow_popover: toolbar::ArrowPopover::default(),
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
    pub fn render(&self) -> scrozz_core::Result<RevisionedFrame> {
        let revision = self.state.revision();
        RevisionedFrame::from_document(self.document(), revision)
    }

    /// Opens the anchored quick-colour palette.
    pub fn open_color_popover(&mut self) {
        self.color_popover.open();
    }

    /// Opens the egui custom-colour fallback.
    pub fn open_custom_color_fallback(&mut self) {
        self.color_popover.open_fallback();
    }

    /// Closes the colour palette after a native picker takes over.
    pub fn native_color_picker_started(&mut self) {
        self.color_popover.close();
    }

    /// Applies a colour returned by a platform picker.
    pub fn apply_external_color(&mut self, color: scrozz_annotate::Color) {
        if self.state.stroke_color() != color {
            self.state.set_stroke_color(color);
        }
    }

    /// Loads persisted custom colours in most-recently-used order.
    pub fn set_custom_swatches(&mut self, colors: Vec<scrozz_annotate::Color>) {
        self.color_popover.set_custom(colors);
    }

    /// Remembers the final system-picker colour, replacing the selected custom
    /// swatch when the picker was opened from one.
    pub fn remember_external_color(&mut self, color: scrozz_annotate::Color) {
        self.apply_external_color(color);
        self.color_popover.remember(color);
    }

    /// Remembers a custom colour without applying it to a newly selected object.
    pub fn remember_custom_color(&mut self, color: scrozz_annotate::Color) {
        self.color_popover.remember(color);
    }

    /// Current custom colours in most-recently-used order.
    #[must_use]
    pub fn custom_swatches(&self) -> &[scrozz_annotate::Color] {
        self.color_popover.custom()
    }

    /// Takes a custom-palette persistence update.
    pub fn take_custom_swatches_change(&mut self) -> Option<Vec<scrozz_annotate::Color>> {
        self.color_popover.take_change()
    }

    /// Whether either anchored colour surface is showing.
    #[must_use]
    pub const fn color_popover_is_open(&self) -> bool {
        self.color_popover.is_open()
    }

    /// The anchored popup's most recently resolved screen rectangle.
    #[must_use]
    pub const fn color_popover_rect(&self) -> Option<egui::Rect> {
        self.color_popover.last_rect()
    }

    /// Opens the arrow style and thickness inspector.
    pub fn open_arrow_popover(&mut self) {
        self.arrow_popover.open();
    }

    /// Whether the arrow inspector is showing.
    #[must_use]
    pub const fn arrow_popover_is_open(&self) -> bool {
        self.arrow_popover.is_open()
    }

    /// The arrow inspector's most recently resolved screen rectangle.
    #[must_use]
    pub const fn arrow_popover_rect(&self) -> Option<egui::Rect> {
        self.arrow_popover.last_rect()
    }

    /// Draws one frame and reports what the host should do.
    pub fn update(&mut self, ui: &mut Ui) -> Intent {
        let theme = theme_for(ui);
        let icons = shared_icons(ui.ctx());
        let surface = crate::paint::Surface::new(&theme, &icons, crate::motion::Motion::at(0.0));

        let inspector_open = self.color_popover.is_open() || self.arrow_popover.is_open();
        let inspector_activation = toolbar::inspector_control_activation(ui);
        if !inspector_open
            && !inspector_activation
            && let Some(intent) = self.keyboard(ui)
        {
            self.pending = Some(intent);
        }

        let full = ui.available_rect_before_wrap();
        let (bar, canvas) = editor_layout(full);

        let view = paint::draw_canvas(
            ui,
            &surface,
            &mut self.state,
            &mut self.preview,
            canvas,
            !inspector_open,
        );
        if let Some(action) = toolbar::draw(
            ui,
            &surface,
            &mut self.state,
            &mut self.color_popover,
            &mut self.arrow_popover,
            bar,
            inspector_open,
        ) {
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

/// The stable toolbar/canvas split. Foreground popups never participate.
#[must_use]
pub fn editor_layout(full: egui::Rect) -> (egui::Rect, egui::Rect) {
    let toolbar_h = toolbar::height_for(full.width());
    let bar = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), toolbar_h));
    let canvas =
        egui::Rect::from_min_max(egui::pos2(full.left(), full.top() + toolbar_h), full.max);
    (bar, canvas)
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

/// Whether an explicit editor action created or reused the stable viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenDisposition {
    /// The viewport was closed.
    FirstOpen,
    /// The stable viewport already existed and will be raised.
    Reused,
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
    pub fn open(&mut self, title: impl Into<String>) -> OpenDisposition {
        let disposition = if self.open {
            OpenDisposition::Reused
        } else {
            OpenDisposition::FirstOpen
        };
        self.title = title.into();
        self.open = true;
        self.focus_requested = true;
        disposition
    }

    /// Closes the window.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Returns keyboard focus after a system picker closes.
    pub fn request_foreground(&mut self) {
        if self.open {
            self.focus_requested = true;
        }
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
        let builder = Self::viewport_builder(title, focus);
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
                Self::emit_foreground(editor_ui.ctx(), focus);
                build(editor_ui);
            },
        );
        if close {
            self.open = false;
        }
    }

    fn viewport_builder(title: String, active: bool) -> egui::ViewportBuilder {
        let builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_app_id("com.thatcube.Scrozz.editor")
            .with_inner_size([1040.0, 720.0])
            .with_min_inner_size(MIN_WINDOW_SIZE)
            .with_resizable(true)
            .with_decorations(true)
            .with_window_level(egui::WindowLevel::Normal);
        if active {
            builder.with_active(true)
        } else {
            builder
        }
    }

    fn emit_foreground(ctx: &egui::Context, requested: bool) {
        if requested {
            // Emitted only in response to Open/Edit or picker focus return. Normal
            // repaints and worker completions do not keep stealing the foreground.
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    #[test]
    fn first_open_and_reuse_both_request_foregrounding() {
        let mut window = EditorWindow::new();
        assert_eq!(window.open("card:1"), OpenDisposition::FirstOpen);
        assert!(std::mem::take(&mut window.focus_requested));

        assert_eq!(window.open("card:1"), OpenDisposition::Reused);
        assert!(std::mem::take(&mut window.focus_requested));
    }

    #[test]
    fn the_editor_is_an_explicitly_normal_level_window() {
        let active = EditorWindow::viewport_builder("Annotate".to_owned(), true);
        assert_eq!(active.window_level, Some(egui::WindowLevel::Normal));
        assert_eq!(active.active, Some(true));
        assert_eq!(
            EditorWindow::viewport_builder("Annotate".to_owned(), false).active,
            None
        );
    }

    #[test]
    fn foreground_focus_is_emitted_only_when_requested() {
        let count = |requested| {
            let ctx = egui::Context::default();
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                EditorWindow::emit_foreground(ui.ctx(), requested);
            });
            output.textures_delta.clear();
            output
                .viewport_output
                .into_values()
                .flat_map(|viewport| viewport.commands)
                .filter(|command| matches!(command, egui::ViewportCommand::Focus))
                .count()
        };
        assert_eq!(count(false), 0);
        assert_eq!(count(true), 1);
    }

    #[test]
    fn repainting_does_not_rearm_foreground_focus() {
        let mut window = EditorWindow::new();
        let _ = window.open("card:1");
        assert!(std::mem::take(&mut window.focus_requested));
        assert!(!window.focus_requested);
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
