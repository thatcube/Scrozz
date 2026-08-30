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
    CROP_SNAP_TOLERANCE, Caret, Command, CropAspect, EditorState, Handle, Intent, MAX_ZOOM,
    MIN_DRAG, MIN_SIZE, MIN_ZOOM, NUDGE, NUDGE_COARSE, SceneDraft, SmartFrameDraft, TextEdit, Tool,
    ZOOM_STEP,
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
    pending: Option<Intent>,
    /// Whether the Scene side panel is shown for this editor session.
    show_smart_frame: bool,
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
            show_smart_frame: false,
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

    // ── Smart Frame public API ──────────────────────────────────

    /// Delivers a completed Smart Frame analysis to the editor.
    ///
    /// The host calls this from a background thread completion handler with
    /// the `revision` tag that was in the [`Intent::AnalyzeSmartFrame`] it
    /// acted on. Stale or cancelled results are silently dropped.
    pub fn deliver_analysis(
        &mut self,
        revision: u64,
        result: std::result::Result<scrozz_annotate::SmartFrameAnalysis, String>,
    ) {
        self.state.finish_smart_frame_analysis(revision, result);
    }

    /// Delivers reviewed sensitive-region suggestions.
    pub fn deliver_sensitive_review(&mut self, review: scrozz_annotate::SensitiveRegionReview) {
        self.state.set_sensitive_review(review);
    }

    /// Enables or disables the Smart Frame side panel.
    ///
    /// The host calls this to show the panel when the user wants to frame their
    /// capture. It is also set automatically when the one-click button in the
    /// panel is pressed.
    pub fn set_smart_frame_visible(&mut self, visible: bool) {
        self.show_smart_frame = visible;
    }

    /// Enables or disables the session-local Scene panel.
    pub fn set_scene_visible(&mut self, visible: bool) {
        self.set_smart_frame_visible(visible);
    }

    /// Whether the Smart Frame side panel is visible.
    #[must_use]
    pub const fn smart_frame_visible(&self) -> bool {
        self.show_smart_frame
    }

    /// Whether the Scene panel is visible in this editor session.
    #[must_use]
    pub const fn scene_visible(&self) -> bool {
        self.smart_frame_visible()
    }

    /// Draws one frame and reports what the host should do.
    pub fn update(&mut self, ui: &mut Ui) -> Intent {
        let theme = theme_for(ui);
        let icons = shared_icons(ui.ctx());
        let surface = crate::paint::Surface::new(&theme, &icons, crate::motion::Motion::at(0.0));

        let inspector_was_open = self.color_popover.is_open() || self.arrow_popover.is_open();
        let inspector_activation = toolbar::inspector_control_activation(ui);

        let full = ui.available_rect_before_wrap();
        let show_sf_panel = self.show_smart_frame || self.state.has_smart_frame_draft();
        let (bar, canvas, sf_panel) = editor_layout_with_smart_frame(full, show_sf_panel);

        let view = paint::draw_canvas(
            ui,
            &surface,
            &mut self.state,
            &mut self.preview,
            canvas,
            !inspector_was_open,
        );
        if let Some(action) = toolbar::draw(
            ui,
            &surface,
            &mut self.state,
            &mut self.color_popover,
            &mut self.arrow_popover,
            bar,
            inspector_was_open,
        ) {
            if action == Intent::ToggleSmartFrame {
                if self.state.has_smart_frame_draft() {
                    self.show_smart_frame = true;
                } else if self.show_smart_frame {
                    self.show_smart_frame = false;
                } else {
                    self.show_smart_frame = true;
                    self.state.edit_existing_scene();
                }
            } else {
                self.pending = Some(action);
            }
        }
        // Smart Frame right panel.
        if let Some(panel_rect) = sf_panel
            && let Some(sf_intent) =
                toolbar::draw_scene_panel(ui, &surface, &mut self.state, panel_rect)
        {
            self.pending = Some(sf_intent);
        }
        toolbar::draw_view_controls(ui, &surface, &mut self.state, canvas, &view);
        let inspector_open = self.color_popover.is_open() || self.arrow_popover.is_open();
        if !inspector_was_open
            && !inspector_open
            && !inspector_activation
            && let Some(intent) = self.keyboard(ui)
        {
            self.pending = Some(intent);
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

/// Width of the compact Scene side panel in points.
const SMART_FRAME_PANEL_W: f32 = 320.0;

/// Layout with an optional Smart Frame right panel.
#[must_use]
fn editor_layout_with_smart_frame(
    full: egui::Rect,
    show_panel: bool,
) -> (egui::Rect, egui::Rect, Option<egui::Rect>) {
    let toolbar_h = toolbar::height_for(full.width());
    let bar = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), toolbar_h));
    let below = egui::Rect::from_min_max(egui::pos2(full.left(), full.top() + toolbar_h), full.max);
    if show_panel && below.width() > SMART_FRAME_PANEL_W + 200.0 {
        let panel_left = below.right() - SMART_FRAME_PANEL_W;
        let canvas = egui::Rect::from_min_max(below.min, egui::pos2(panel_left, below.bottom()));
        let panel = egui::Rect::from_min_max(egui::pos2(panel_left, below.top()), below.max);
        (bar, canvas, Some(panel))
    } else {
        (bar, below, None)
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
///
/// One instance exists per open editor, not one for the whole app: several
/// captures may each have their own editor open at once, and each needs its
/// own stable [`egui::ViewportId`] or egui would collapse them into a single
/// viewport and one card's editor would silently steal another's window.
#[derive(Debug)]
pub struct EditorWindow {
    id: egui::ViewportId,
    open: bool,
    focus_requested: bool,
    confirm_discard: bool,
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

/// What the user decided about the document this frame.
///
/// Returned by [`EditorWindow::show`] (and [`EditorWindow::request_close`])
/// so the caller — which alone knows how to render, persist and refresh a
/// card's thumbnail — can act on the decision without `EditorWindow` needing
/// to know anything about documents at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorWindowExit {
    /// Nothing was decided; the window stays open.
    None,
    /// Commit: render the document, persist it, and refresh the card's
    /// thumbnail. The window has already closed.
    Done,
    /// Discard: close without touching the card. Reached by an explicit
    /// Cancel, by a clean (non-dirty) close of any kind, or by choosing
    /// "Discard" from the confirm-discard prompt.
    Cancel,
}

/// The outcome of the confirm-discard prompt for one frame.
///
/// A tri-state rather than a plain `bool`, and returned from a free function
/// rather than a method: the prompt is drawn from inside the viewport
/// closure below, which has already borrowed pieces of `self` by value (not
/// `&mut self`) to draw at all, so it cannot clear `confirm_discard` on
/// `self` directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscardPromptOutcome {
    /// Still showing; no button has been clicked yet.
    Waiting,
    /// "Keep Editing": stay open, dismiss the prompt.
    KeepEditing,
    /// "Discard": close without saving.
    Discard,
}

impl EditorWindow {
    /// A closed editor window identified by `id`.
    ///
    /// `id` distinguishes this window's viewport from every other editor's —
    /// callers pass their card's own identity so two cards never collide on
    /// the same viewport, while the same card's window stays stable across
    /// repeated opens.
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self {
            id: egui::ViewportId::from_hash_of(("scrozz-editor", id)),
            open: false,
            focus_requested: false,
            confirm_discard: false,
            title: String::new(),
        }
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
        // A freshly (re)opened session never starts behind a stale prompt.
        self.confirm_discard = false;
        disposition
    }

    /// Closes the window.
    pub fn close(&mut self) {
        self.open = false;
        self.confirm_discard = false;
    }

    /// Reopens the window immediately after [`Self::show`] returned
    /// [`EditorWindowExit::Done`] but the caller could not actually commit
    /// the render, so the window must not appear to have closed at all.
    ///
    /// `show` already closed the native viewport for this frame before
    /// returning `Done` (its own doc comment says as much), so nothing here
    /// undoes that first close -- it only asks the *next* frame's `show`
    /// call to draw the same viewport again, which looks identical to the
    /// window never having closed. Kept free of a title argument on
    /// purpose: the caller only just learned the close needs to be undone,
    /// not what the window used to be titled, and the title is unchanged.
    pub fn reopen(&mut self) {
        self.open = true;
        self.focus_requested = true;
        self.confirm_discard = false;
    }

    /// Returns keyboard focus after a system picker closes.
    pub fn request_foreground(&mut self) {
        if self.open {
            self.focus_requested = true;
        }
    }

    /// Asks the window to close exactly as its own native close button would.
    ///
    /// Shared by the native close button (handled inside [`Self::show`]) and
    /// by `Escape` (`Intent::Close`), so a keyboard dismissal can never
    /// bypass the same no-silent-loss guarantee a click on the close button
    /// gets: a clean document closes immediately and reports [`EditorWindowExit::Cancel`];
    /// a dirty one is held open behind the confirm-discard prompt and
    /// reports [`EditorWindowExit::None`] until the prompt is resolved on a
    /// later call to [`Self::show`].
    pub fn request_close(&mut self, dirty: bool) -> EditorWindowExit {
        if !dirty {
            self.close();
            return EditorWindowExit::Cancel;
        }
        if !self.confirm_discard {
            self.confirm_discard = true;
            self.focus_requested = true;
        }
        EditorWindowExit::None
    }

    /// Shows the window, calling `build` to draw its contents.
    ///
    /// `dirty` says whether the document has unconfirmed changes: a clean
    /// native close is harmless and closes immediately, a dirty one is held
    /// open behind a confirm-discard prompt instead of being lost silently.
    ///
    /// Draws Done/Cancel chrome above `build`'s content whenever the prompt
    /// is not showing. Cancel and Done are both unconditional and need no
    /// confirmation of their own — they are the user's own explicit choice,
    /// unlike a native close or `Escape`, which might be an absent-minded
    /// reflex on a document the user still wants.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        dirty: bool,
        build: impl FnMut(&mut Ui),
    ) -> EditorWindowExit {
        if !self.open {
            return EditorWindowExit::None;
        }
        let focus = std::mem::take(&mut self.focus_requested);
        let title = if self.title.is_empty() {
            "Annotate".to_owned()
        } else {
            format!("Annotate — {}", self.title)
        };
        let builder = Self::viewport_builder(title, focus);
        let confirm_discard = self.confirm_discard;
        let mut chrome_exit = EditorWindowExit::None;
        let mut native_close_requested = false;
        let mut prompt_outcome = DiscardPromptOutcome::Waiting;
        let mut build = build;
        ctx.show_viewport_immediate(self.id, builder, |editor_ui, _class| {
            native_close_requested = editor_ui
                .ctx()
                .input(|input| input.viewport().close_requested());
            Self::emit_foreground(editor_ui.ctx(), focus);
            if confirm_discard {
                prompt_outcome = Self::show_confirm_discard(editor_ui);
            } else {
                Self::show_done_cancel_chrome(editor_ui, &mut chrome_exit);
                build(editor_ui);
            }
        });

        let was_prompting = confirm_discard;
        let exit = Self::reconcile(
            dirty,
            native_close_requested,
            chrome_exit,
            prompt_outcome,
            &mut self.confirm_discard,
        );
        // Focus follows only the *transition* into the prompt, not every
        // repaint while it's already showing — mirroring `emit_foreground`'s
        // own "not on every repaint" rule.
        if !was_prompting && self.confirm_discard {
            self.focus_requested = true;
        }
        if matches!(exit, EditorWindowExit::Done | EditorWindowExit::Cancel) {
            self.close();
        }
        exit
    }

    /// Combines one frame's raw signals into the window's exit decision.
    ///
    /// Pure and separate from [`Self::show`]'s viewport plumbing so the full
    /// decision table — chrome buttons, native close, and the confirm-discard
    /// prompt, each possibly firing on the same frame — can be exercised
    /// directly in tests, without rendering or simulating a real button
    /// click through egui's immediate-viewport machinery.
    fn reconcile(
        dirty: bool,
        native_close_requested: bool,
        chrome_exit: EditorWindowExit,
        prompt_outcome: DiscardPromptOutcome,
        confirm_discard: &mut bool,
    ) -> EditorWindowExit {
        let mut exit = chrome_exit;
        match prompt_outcome {
            DiscardPromptOutcome::Waiting => {}
            DiscardPromptOutcome::KeepEditing => *confirm_discard = false,
            DiscardPromptOutcome::Discard => {
                *confirm_discard = false;
                exit = EditorWindowExit::Cancel;
            }
        }
        if matches!(exit, EditorWindowExit::None) && native_close_requested {
            if dirty {
                *confirm_discard = true;
            } else {
                exit = EditorWindowExit::Cancel;
            }
        }
        exit
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

    /// Draws the Done/Cancel chrome above the editor's own content.
    fn show_done_cancel_chrome(ui: &mut Ui, exit: &mut EditorWindowExit) {
        egui::Panel::top("scrozz-editor-chrome").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    *exit = EditorWindowExit::Cancel;
                }
                if ui.button("Done").clicked() {
                    *exit = EditorWindowExit::Done;
                }
            });
        });
    }

    /// Draws the "discard your changes?" prompt in place of the editor.
    fn show_confirm_discard(ui: &mut Ui) -> DiscardPromptOutcome {
        let mut outcome = DiscardPromptOutcome::Waiting;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.heading("Discard your changes?");
                ui.label("Closing now will lose the annotations made in this session.");
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 240.0).max(0.0) / 2.0);
                    if ui.button("Keep Editing").clicked() {
                        outcome = DiscardPromptOutcome::KeepEditing;
                    }
                    if ui.button("Discard").clicked() {
                        outcome = DiscardPromptOutcome::Discard;
                    }
                });
            });
        });
        outcome
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    #[test]
    fn first_open_and_reuse_both_request_foregrounding() {
        let mut window = EditorWindow::new(1);
        assert_eq!(window.open("card:1"), OpenDisposition::FirstOpen);
        assert!(std::mem::take(&mut window.focus_requested));

        assert_eq!(window.open("card:1"), OpenDisposition::Reused);
        assert!(std::mem::take(&mut window.focus_requested));
    }

    #[test]
    fn two_windows_get_distinct_viewport_ids() {
        // The whole point of taking an id: two cards editing at once must
        // never collapse onto the same egui viewport.
        let a = EditorWindow::new(1);
        let b = EditorWindow::new(2);
        assert_ne!(a.id, b.id);
        // And the same id is stable across constructions.
        assert_eq!(EditorWindow::new(7).id, EditorWindow::new(7).id);
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
        let mut window = EditorWindow::new(1);
        let _ = window.open("card:1");
        assert!(std::mem::take(&mut window.focus_requested));
        assert!(!window.focus_requested);
    }

    #[test]
    fn reopen_undoes_a_close_show_already_committed_for_this_frame() {
        // `show` closes the viewport itself the instant it decides Done or
        // Cancel, before the caller ever sees the exit value -- so this is
        // the only lever a caller has to say "actually, keep going" when a
        // Done's render turned out to fail. It must look exactly like the
        // window never closed: open again, refocused, and with no stale
        // discard prompt left over from whatever `show` was doing.
        let mut window = EditorWindow::new(1);
        let _ = window.open("card:1");
        std::mem::take(&mut window.focus_requested);
        window.confirm_discard = true;
        window.close();
        assert!(!window.is_open());

        window.reopen();
        assert!(window.is_open());
        assert!(std::mem::take(&mut window.focus_requested));
        assert!(!window.confirm_discard);
    }

    #[test]
    fn a_plain_frame_with_nothing_clicked_stays_open_and_undecided() {
        // `reconcile`'s own tests below cover every decision branch as pure
        // state transitions. `show()` itself is exercised here only for the
        // one frame shape that's safe to run through the real (embedded,
        // backend-less) viewport in a unit test: nothing clicked, no native
        // close. Simulating an actual button click or a native close signal
        // would require driving egui's pointer/viewport-info input state
        // through the immediate-viewport machinery frame-by-frame, which
        // has no reliable headless harness in this codebase (confirmed: a
        // `ViewportCommand::Close` sent by the app is the *opposite*
        // direction of a real close request — it does not set
        // `close_requested()`, which only a live backend can do). The whole
        // call is wrapped in `run_ui` because `show_viewport_immediate`
        // needs a pass already under way (fonts installed etc.) — calling it
        // against a bare, never-run `Context` panics.
        let mut window = EditorWindow::new(1);
        let _ = window.open("card:1");
        let ctx = egui::Context::default();
        let mut exit = EditorWindowExit::None;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            exit = window.show(ui.ctx(), true, |ui| {
                ui.label("editing");
            });
        });
        output.textures_delta.clear();
        assert_eq!(exit, EditorWindowExit::None);
        assert!(window.is_open());
    }

    #[test]
    fn reconcile_lets_an_unconditional_done_click_win() {
        let mut confirm_discard = false;
        let exit = EditorWindow::reconcile(
            true,
            false,
            EditorWindowExit::Done,
            DiscardPromptOutcome::Waiting,
            &mut confirm_discard,
        );
        assert_eq!(exit, EditorWindowExit::Done);
        assert!(!confirm_discard);
    }

    #[test]
    fn reconcile_lets_an_unconditional_cancel_click_win() {
        let mut confirm_discard = false;
        let exit = EditorWindow::reconcile(
            true,
            false,
            EditorWindowExit::Cancel,
            DiscardPromptOutcome::Waiting,
            &mut confirm_discard,
        );
        assert_eq!(exit, EditorWindowExit::Cancel);
    }

    #[test]
    fn reconcile_closes_a_clean_document_on_native_close_without_prompting() {
        let mut confirm_discard = false;
        let exit = EditorWindow::reconcile(
            false,
            true,
            EditorWindowExit::None,
            DiscardPromptOutcome::Waiting,
            &mut confirm_discard,
        );
        assert_eq!(exit, EditorWindowExit::Cancel);
        assert!(!confirm_discard);
    }

    #[test]
    fn reconcile_holds_a_dirty_document_open_behind_the_prompt_on_native_close() {
        let mut confirm_discard = false;
        let exit = EditorWindow::reconcile(
            true,
            true,
            EditorWindowExit::None,
            DiscardPromptOutcome::Waiting,
            &mut confirm_discard,
        );
        assert_eq!(exit, EditorWindowExit::None);
        assert!(confirm_discard);
    }

    #[test]
    fn reconcile_repeated_native_close_while_prompting_stays_armed_and_open() {
        let mut confirm_discard = true;
        let exit = EditorWindow::reconcile(
            true,
            true,
            EditorWindowExit::None,
            DiscardPromptOutcome::Waiting,
            &mut confirm_discard,
        );
        assert_eq!(exit, EditorWindowExit::None);
        assert!(confirm_discard);
    }

    #[test]
    fn reconcile_keep_editing_clears_the_prompt_and_stays_open() {
        let mut confirm_discard = true;
        let exit = EditorWindow::reconcile(
            true,
            false,
            EditorWindowExit::None,
            DiscardPromptOutcome::KeepEditing,
            &mut confirm_discard,
        );
        assert_eq!(exit, EditorWindowExit::None);
        assert!(!confirm_discard);
    }

    #[test]
    fn reconcile_discard_from_the_prompt_closes_without_committing() {
        let mut confirm_discard = true;
        let exit = EditorWindow::reconcile(
            true,
            false,
            EditorWindowExit::None,
            DiscardPromptOutcome::Discard,
            &mut confirm_discard,
        );
        assert_eq!(exit, EditorWindowExit::Cancel);
        assert!(!confirm_discard);
    }

    #[test]
    fn request_close_mirrors_native_close_semantics() {
        let mut clean = EditorWindow::new(1);
        let _ = clean.open("card:1");
        assert_eq!(clean.request_close(false), EditorWindowExit::Cancel);
        assert!(!clean.is_open());

        let mut dirty = EditorWindow::new(2);
        let _ = dirty.open("card:2");
        assert_eq!(dirty.request_close(true), EditorWindowExit::None);
        assert!(dirty.is_open());
        assert!(dirty.confirm_discard);

        // A repeated request while already prompting does not re-arm or
        // otherwise change anything.
        assert_eq!(dirty.request_close(true), EditorWindowExit::None);
        assert!(dirty.confirm_discard);
    }

    #[test]
    fn a_fresh_open_never_starts_behind_a_stale_prompt() {
        let mut window = EditorWindow::new(1);
        let _ = window.open("card:1");
        window.confirm_discard = true;
        let _ = window.open("card:1");
        assert!(!window.confirm_discard);
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

/// Places a document at an absolute screen-points-per-document-point zoom.
#[must_use]
pub fn fit_absolute(
    content: LogicalRect,
    area: egui::Rect,
    zoom: f32,
    pan: (f32, f32),
) -> egui::Rect {
    let (w, h) = (content.size.width as f32, content.size.height as f32);
    if w <= 0.0 || h <= 0.0 || area.width() <= 0.0 || area.height() <= 0.0 {
        return egui::Rect::from_min_size(area.center(), egui::Vec2::ZERO);
    }
    let zoom = if zoom.is_finite() {
        zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        1.0
    };
    let size = egui::vec2(w * zoom, h * zoom);
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
    /// Command/Ctrl: temporarily disable crop-edge snapping.
    pub disable_crop_snap: bool,
}

impl EditorModifiers {
    /// Reads the modifiers from this frame's input.
    #[must_use]
    pub fn read(ui: &Ui) -> Self {
        ui.ctx().input(|input| Self {
            constrain: input.modifiers.shift,
            pan: input.key_down(Key::Space),
            disable_crop_snap: input.modifiers.command,
        })
    }
}

const _: fn(&Modifiers) = |_| {};
