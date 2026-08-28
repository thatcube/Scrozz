//! The editor canvas: the live preview, the drag chrome and the handles.
//!
//! # Two layers, deliberately
//!
//! The **image** layer is a texture produced by [`SkiaRenderer`] — the same
//! renderer the export uses — showing the document with every annotation
//! already composited. The **chrome** layer is drawn with egui's painter on top
//! and contains only things that must never appear in the export: selection
//! handles, the crop scrim, the hover outline.
//!
//! Drawing annotations with egui instead would be far simpler and would be
//! wrong. egui has no blur and no pixelate, so a redaction — the one annotation
//! where being wrong actually matters — would preview as a plain grey box while
//! the file on disk got something else. Routing the preview through the
//! renderer makes "what you see" and "what you export" the same code path by
//! construction rather than by discipline.

use egui::{Color32, CursorIcon, Painter, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use scrozz_annotate::{Annotation, SkiaRenderer, font};
use scrozz_core::{LogicalPoint, LogicalRect, ScaleFactor};

use crate::paint::{Surface, focus_ring};
use crate::theme::{Palette, Radius, Space, corner};

use super::state::{EditorState, HANDLE_RADIUS, Handle, Tool};
use super::{EditorModifiers, rect_to_screen, to_document};

/// The largest preview texture edge, in pixels.
///
/// A 6K capture is ~24 MP; uploading that every time an arrow moves would stall
/// the frame and exhaust texture memory for no visible gain, since the canvas
/// is at most ~2000 px across. The export is unaffected — it renders from the
/// document at full scale, never from this texture.
pub const MAX_PREVIEW_PX: u32 = 2400;

/// A cached render of the document.
///
/// Keyed on [`EditorState::revision`], which changes on every mutation, so an
/// idle editor uploads nothing and a drag uploads once per changed frame.
#[derive(Default)]
pub struct Preview {
    texture: Option<egui::TextureHandle>,
    revision: Option<u64>,
    /// The pixel width the cached texture was rendered at.
    width: u32,
    /// Set once a render fails, to stop retrying it every frame.
    failed: bool,
}

impl std::fmt::Debug for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Preview")
            .field("revision", &self.revision)
            .field("width", &self.width)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl Preview {
    /// Forgets the cached texture, forcing a re-render.
    pub fn invalidate(&mut self) {
        self.revision = None;
        self.failed = false;
    }

    /// The cached texture, rendering it first if it is stale.
    fn texture(
        &mut self,
        ctx: &egui::Context,
        state: &EditorState,
        target_px: u32,
    ) -> Option<&egui::TextureHandle> {
        let fresh = self.revision == Some(state.revision()) && self.width == target_px;
        if !fresh && !self.failed {
            match render(state, target_px) {
                Ok(image) => {
                    let handle = ctx.load_texture(
                        "scrozz-editor-preview",
                        image,
                        egui::TextureOptions::LINEAR,
                    );
                    self.texture = Some(handle);
                    self.revision = Some(state.revision());
                    self.width = target_px;
                }
                Err(error) => {
                    tracing::warn!(%error, "editor preview render failed");
                    self.failed = true;
                }
            }
        }
        self.texture.as_ref()
    }
}

/// Renders the document to a colour image at `target_px` wide.
fn render(state: &EditorState, target_px: u32) -> scrozz_core::Result<egui::ColorImage> {
    let frame = SkiaRenderer.render_to_width(state.document(), target_px.max(1))?;
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let mut pixels = Vec::with_capacity(w * h);
    for y in 0..h {
        let row = &frame.data[y * frame.stride..];
        for x in 0..w {
            let p = &row[x * 4..x * 4 + 4];
            // The renderer emits straight (un-premultiplied) RGBA; egui's
            // `Color32::from_rgba_unmultiplied` premultiplies for us.
            pixels.push(Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]));
        }
    }
    Ok(egui::ColorImage {
        size: [w, h],
        pixels,
        source_size: egui::vec2(w as f32, h as f32),
    })
}

/// Where the document ended up on screen, and what the pointer did.
#[derive(Debug, Clone, Copy)]
pub struct CanvasView {
    /// The document's rectangle on screen.
    pub image: Rect,
    /// The whole canvas area.
    pub area: Rect,
    /// The document rectangle being shown, in logical points.
    pub content: LogicalRect,
    /// Whether the pointer is over the canvas.
    pub hovered: bool,
}

impl CanvasView {
    /// The scale from logical points to screen points.
    #[must_use]
    pub fn scale(&self) -> f32 {
        if self.content.size.width <= 0.0 {
            1.0
        } else {
            self.image.width() / self.content.size.width as f32
        }
    }
}

/// Draws the canvas and drives the pointer gestures on it.
pub fn draw_canvas(
    ui: &mut Ui,
    surface: &Surface<'_>,
    state: &mut EditorState,
    preview: &mut Preview,
    area: Rect,
) -> CanvasView {
    let palette = surface.palette();
    let painter = ui.painter_at(area);
    checkerboard(&painter, area, palette);

    let content = state.document().content_bounds();
    let image = super::fit(content, area.shrink(Space::LG), state.zoom(), state.pan());
    let view = CanvasView {
        image,
        area,
        content,
        hovered: false,
    };
    // Hand the layout back to the state so screen-space hit tolerances survive
    // zoom. Done before `gestures` so the very first press of a frame is judged
    // against the scale that frame is actually drawn at.
    state.set_view_scale(f64::from(view.scale()));

    let target_px = preview_width(image, ui.ctx().pixels_per_point());
    if let Some(texture) = preview.texture(ui.ctx(), state, target_px) {
        painter.image(
            texture.id(),
            image,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        painter.rect_filled(image, corner(Radius::CARD), palette.card_fill);
    }
    painter.rect_stroke(
        image,
        corner(0.0),
        Stroke::new(1.0, palette.hairline),
        StrokeKind::Outside,
    );

    let response = ui.interact(area, ui.id().with("editor-canvas"), Sense::click_and_drag());
    let view = CanvasView {
        hovered: response.hovered(),
        ..view
    };
    gestures(ui, state, &response, &view);

    let chrome = ui.painter_at(area);
    draw_crop_scrim(&chrome, state, &view, palette);
    draw_selection(&chrome, state, &view, palette);
    draw_caret(ui, &chrome, state, &view, palette);
    cursor(ui, state, &response);
    view
}

/// Draws the text caret and tells the platform where to put its IME window.
///
/// Publishing [`IMEOutput`](egui::output::IMEOutput) is what makes `egui_winit`
/// call `set_ime_allowed(true)`, so this is not merely cosmetic: without it the
/// OS never starts a composition and CJK input is impossible.
fn draw_caret(
    ui: &Ui,
    painter: &Painter,
    state: &EditorState,
    view: &CanvasView,
    palette: &Palette,
) {
    let Some(id) = state.editing_text() else {
        return;
    };
    let Some((row, column)) = state.text_caret_cell() else {
        return;
    };
    let Some(object) = state.document().annotations().iter().find(|o| o.id == id) else {
        return;
    };
    let Annotation::Text { at, .. } = &object.annotation else {
        return;
    };
    let size = object.style.effective_font_size();
    // The built-in font is monospaced, so the caret's column is an exact
    // multiple of the advance; no shaping pass is needed to place it.
    let x = at.x + column as f64 * font::ADVANCE * size;
    let y = at.y + row as f64 * font::LINE_HEIGHT * size;
    let top = super::to_screen(LogicalPoint::new(x, y), view.image, view.content);
    let bottom = super::to_screen(LogicalPoint::new(x, y + size), view.image, view.content);
    let caret = Rect::from_min_max(top, bottom);

    // Blink on the same cadence as the rest of the system, and keep the frame
    // loop alive so it actually blinks while the pointer is still.
    let blink = 1.06;
    let phase = ui.input(|i| i.time).rem_euclid(blink);
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(
            ((blink - phase) * 1000.0).max(16.0) as u64,
        ));
    if phase < blink / 2.0 {
        painter.rect_filled(
            Rect::from_min_max(caret.min, caret.max + egui::vec2(1.5, 0.0)),
            corner(0.0),
            palette.accent,
        );
    }

    ui.ctx().output_mut(|out| {
        out.ime = Some(egui::output::IMEOutput {
            rect: caret.expand(2.0),
            cursor_rect: caret,
            purpose: egui::IMEPurpose::Normal,
            // The caret only ever moves here by the user's own action, so a
            // composition in flight is never stale enough to need interrupting.
            should_interrupt_composition: false,
        });
    });
}

/// The width, in pixels, the preview should be rendered at.
fn preview_width(image: Rect, ppp: f32) -> u32 {
    let want = (image.width() * ppp).ceil().max(1.0) as u32;
    // Quantise so that a one-pixel window resize does not re-render: without
    // this, dragging the window edge re-composites the whole capture on every
    // frame of the drag.
    let quantised = want.div_ceil(64) * 64;
    quantised.clamp(64, MAX_PREVIEW_PX)
}

/// Translates pointer events into state-machine calls.
fn gestures(ui: &Ui, state: &mut EditorState, response: &egui::Response, view: &CanvasView) {
    let modifiers = EditorModifiers::read(ui);
    let Some(screen) = response.interact_pointer_pos().or(response.hover_pos()) else {
        return;
    };
    let point = to_document(screen, view.image, view.content);

    if response.drag_started() {
        if modifiers.pan {
            state.begin_pan((screen.x, screen.y));
        } else {
            state.pointer_pressed(point);
        }
    } else if response.dragged() {
        if modifiers.pan {
            state.pan_to((screen.x, screen.y));
        } else {
            state.pointer_dragged(point, modifiers.constrain);
        }
    } else if response.drag_stopped() {
        state.pointer_released();
    } else if response.clicked() && !modifiers.pan {
        // A click with no drag never reaches drag_started, so place-tools and
        // click-to-select would do nothing at all without this.
        state.pointer_pressed(point);
        state.pointer_released();
    }

    if response.hovered() {
        let scroll = ui.ctx().input(|input| input.smooth_scroll_delta.y);
        if ui.ctx().input(|input| input.modifiers.command) && scroll.abs() > 0.0 {
            state.set_zoom(state.zoom() * (1.0 + scroll * 0.002));
        }
    }
}

/// Dims everything outside the crop being dragged.
fn draw_crop_scrim(
    painter: &egui::Painter,
    state: &EditorState,
    view: &CanvasView,
    palette: &Palette,
) {
    let Some(rect) = state.pending_crop() else {
        if state.tool() == Tool::Crop
            && let Some(existing) = state.document().crop()
        {
            let screen = rect_to_screen(existing, view.image, view.content);
            painter.rect_stroke(
                screen,
                corner(0.0),
                Stroke::new(1.0, palette.accent),
                StrokeKind::Outside,
            );
        }
        return;
    };
    let keep = rect_to_screen(rect, view.image, view.content);
    let scrim = Color32::from_black_alpha(120);
    for band in [
        Rect::from_min_max(view.image.min, pos2(view.image.right(), keep.top())),
        Rect::from_min_max(pos2(view.image.left(), keep.bottom()), view.image.max),
        Rect::from_min_max(pos2(view.image.left(), keep.top()), keep.left_bottom()),
        Rect::from_min_max(keep.right_top(), pos2(view.image.right(), keep.bottom())),
    ] {
        if band.is_positive() {
            painter.rect_filled(band, corner(0.0), scrim);
        }
    }
    painter.rect_stroke(
        keep,
        corner(0.0),
        Stroke::new(1.0, Color32::WHITE),
        StrokeKind::Outside,
    );
    thirds(painter, keep);
}

/// The rule-of-thirds guides inside a crop.
fn thirds(painter: &egui::Painter, rect: Rect) {
    let guide = Stroke::new(1.0, Color32::from_white_alpha(70));
    for i in 1..3 {
        let f = i as f32 / 3.0;
        let x = rect.left() + rect.width() * f;
        let y = rect.top() + rect.height() * f;
        painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], guide);
        painter.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)], guide);
    }
}

/// Draws the selection outline and its eight resize handles.
fn draw_selection(
    painter: &egui::Painter,
    state: &EditorState,
    view: &CanvasView,
    palette: &Palette,
) {
    let Some(bounds) = state.selection_bounds() else {
        return;
    };
    let rect = rect_to_screen(bounds, view.image, view.content).expand(2.0);
    // A dark under-stroke keeps the selection visible over a light capture, and
    // the accent over a dark one; a single colour disappears against one or the
    // other every time.
    painter.rect_stroke(
        rect,
        corner(0.0),
        Stroke::new(3.0, Color32::from_black_alpha(90)),
        StrokeKind::Outside,
    );
    painter.rect_stroke(
        rect,
        corner(0.0),
        Stroke::new(1.5, palette.accent),
        StrokeKind::Outside,
    );

    let r = HANDLE_RADIUS as f32;
    for handle in Handle::ALL {
        let at = super::to_screen(handle.position(&bounds), view.image, view.content);
        let at = pos2(
            at.x + handle_bias(handle.moves_left(), handle.moves_right()) * 2.0,
            at.y + handle_bias(handle.moves_top(), handle.moves_bottom()) * 2.0,
        );
        painter.circle_filled(at, r + 1.0, Color32::from_black_alpha(70));
        painter.circle_filled(at, r, Color32::WHITE);
        painter.circle_stroke(at, r, Stroke::new(1.0, palette.accent));
    }
}

const fn handle_bias(low: bool, high: bool) -> f32 {
    if low {
        -1.0
    } else if high {
        1.0
    } else {
        0.0
    }
}

/// Sets the pointer cursor to match what a click would do.
fn cursor(ui: &Ui, state: &EditorState, response: &egui::Response) {
    if !response.hovered() {
        return;
    }
    let icon = match state.tool() {
        Tool::Select => CursorIcon::Default,
        Tool::Text => CursorIcon::Text,
        Tool::Crop => CursorIcon::Crosshair,
        _ => CursorIcon::Crosshair,
    };
    ui.ctx().set_cursor_icon(icon);
}

/// The transparency checkerboard behind the image.
///
/// A capture is opaque, but a *cropped* or beautified one is not — its padding
/// is transparent — and a flat background would make that padding look like a
/// solid colour that would then be missing from the exported PNG.
fn checkerboard(painter: &egui::Painter, area: Rect, palette: &Palette) {
    painter.rect_filled(area, corner(0.0), palette.card_fill);
    let cell = 12.0;
    let alt = if palette.appearance == crate::theme::Appearance::Dark {
        Color32::from_white_alpha(6)
    } else {
        Color32::from_black_alpha(8)
    };
    let cols = (area.width() / cell).ceil() as i32;
    let rows = (area.height() / cell).ceil() as i32;
    for row in 0..rows {
        for col in 0..cols {
            if (row + col) % 2 == 0 {
                continue;
            }
            let min = pos2(
                area.left() + col as f32 * cell,
                area.top() + row as f32 * cell,
            );
            let tile = Rect::from_min_size(min, vec2(cell, cell)).intersect(area);
            if tile.is_positive() {
                painter.rect_filled(tile, corner(0.0), alt);
            }
        }
    }
}

/// Draws a focus ring around the canvas when it holds keyboard focus.
pub fn draw_focus(painter: &egui::Painter, area: Rect, palette: &Palette) {
    focus_ring(painter, area, Radius::CARD, palette);
}

/// The scale a document should be rendered at to fill `width` pixels.
///
/// Free-standing so the export path can ask the same question without a canvas.
#[must_use]
pub fn scale_for_width(content: LogicalRect, width: u32) -> ScaleFactor {
    if content.size.width <= 0.0 {
        return ScaleFactor::new(1.0);
    }
    ScaleFactor::new(f64::from(width) / content.size.width)
}
