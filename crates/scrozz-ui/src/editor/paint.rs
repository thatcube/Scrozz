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
use scrozz_annotate::{Annotation, ArrowStyle, SkiaRenderer, font};
use scrozz_core::{
    ColorSpace, LogicalPoint, LogicalRect, LogicalSize, PixelFormat, ScaleFactor,
    Transform as ColorTransform,
};

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
const MAX_PREVIEW_COMPOSITE_PIXELS: f64 = 16_777_216.0;

/// A cached render of the document.
///
/// Keyed on [`EditorState::revision`], which changes on every mutation, so an
/// idle editor uploads nothing and a drag uploads once per changed frame.
#[derive(Default)]
pub struct Preview {
    texture: Option<egui::TextureHandle>,
    /// Subject bounds normalised to the complete rendered Scene texture.
    subject: Option<Rect>,
    revision: Option<u64>,
    /// The pixel width the cached texture was rendered at.
    width: u32,
    /// Whether the cached texture shows the uncropped source for crop mode.
    full_document: bool,
    /// The exact render key that failed, to stop retrying only that one every
    /// frame.
    failed: Option<(u64, u32, bool)>,
}

impl std::fmt::Debug for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Preview")
            .field("revision", &self.revision)
            .field("width", &self.width)
            .field("full_document", &self.full_document)
            .field("subject", &self.subject)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl Preview {
    /// Forgets the cached texture, forcing a re-render.
    pub fn invalidate(&mut self) {
        self.revision = None;
        self.failed = None;
    }

    /// The cached texture, rendering it first if it is stale.
    fn texture(
        &mut self,
        ctx: &egui::Context,
        state: &EditorState,
        target_px: u32,
    ) -> Option<(egui::TextureId, Option<Rect>)> {
        let full_document = state.crop_mode();
        let key = (state.revision(), target_px, full_document);
        let fresh = self.revision == Some(key.0)
            && self.width == key.1
            && self.full_document == full_document;
        if !fresh && self.failed != Some(key) {
            match render(state, target_px, full_document) {
                Ok(rendered) => {
                    let handle = ctx.load_texture(
                        "scrozz-editor-preview",
                        rendered.image,
                        egui::TextureOptions::LINEAR,
                    );
                    self.texture = Some(handle);
                    self.subject = rendered.subject;
                    self.revision = Some(key.0);
                    self.width = target_px;
                    self.full_document = full_document;
                    self.failed = None;
                }
                Err(error) => {
                    tracing::warn!(%error, "editor preview render failed");
                    // Never display an old, potentially unredacted revision
                    // after a new one failed.
                    self.texture = None;
                    self.subject = None;
                    self.revision = None;
                    self.width = 0;
                    self.failed = Some(key);
                }
            }
        }
        self.texture
            .as_ref()
            .map(|texture| (texture.id(), self.subject))
    }
}

struct RenderedPreview {
    image: egui::ColorImage,
    subject: Option<Rect>,
}

/// Renders the document to a colour image at `target_px` wide.
///
/// # Why the format and the colour space are both consulted
///
/// The frame comes back in whatever the renderer produced, and getting either
/// wrong is visible rather than theoretical.
///
/// tiny-skia composites in *premultiplied* RGBA, so a 50%-alpha highlight comes
/// out with its channels already scaled. Handing those bytes to
/// [`Color32::from_rgba_unmultiplied`] scales them a second time, and the
/// highlight previews darker than it exports. The format says which it is, so
/// this asks instead of assuming.
///
/// The pixels are also in the *capture's* colour space, which on a modern Mac is
/// Display P3. egui uploads texture bytes and the GPU shows them as sRGB, so P3
/// bytes sent through unchanged preview over-saturated — and would disagree with
/// the exported file, which carries a P3 profile and is therefore correct. They
/// are converted here instead. Only here: the document keeps its own space, so
/// nothing about the export changes.
fn render(
    state: &EditorState,
    target_px: u32,
    full_document: bool,
) -> scrozz_core::Result<RenderedPreview> {
    let mut uncropped;
    let document = if full_document && state.document().crop().is_some() {
        uncropped = state.document().clone();
        uncropped.set_crop(None)?;
        &uncropped
    } else {
        state.document()
    };
    let (frame, layout) = SkiaRenderer.render_to_width_with_layout(document, target_px.max(1))?;
    let subject = layout.map(|layout| {
        Rect::from_min_size(
            pos2(
                (layout.subject.origin.x / f64::from(layout.width)) as f32,
                (layout.subject.origin.y / f64::from(layout.height)) as f32,
            ),
            vec2(
                (layout.subject.size.width / f64::from(layout.width)) as f32,
                (layout.subject.size.height / f64::from(layout.height)) as f32,
            ),
        )
    });
    Ok(RenderedPreview {
        image: to_color_image(&frame),
        subject,
    })
}

/// Turns a rendered frame into what egui uploads, honouring its format and
/// colour space. See [`render`] for why both matter.
#[must_use]
pub fn to_color_image(frame: &scrozz_core::Frame) -> egui::ColorImage {
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let premultiplied = frame.format.is_premultiplied();
    let bgra = matches!(
        frame.format,
        PixelFormat::Bgra8 | PixelFormat::BgraPremultiplied8
    );

    // Display space, not document space. The identity case — an sRGB or unknown
    // capture — costs one branch per pixel and no arithmetic.
    let to_display = ColorTransform::new(frame.color_space, ColorSpace::Srgb);
    let linear = to_display.source_linear_table();

    let mut pixels = Vec::with_capacity(w * h);
    for y in 0..h {
        let row = &frame.data[y * frame.stride..];
        for x in 0..w {
            let p = &row[x * 4..x * 4 + 4];
            let (r, g, b) = if bgra {
                (p[2], p[1], p[0])
            } else {
                (p[0], p[1], p[2])
            };
            let (r, g, b) = if to_display.is_identity() {
                (r, g, b)
            } else {
                // Converting premultiplied channels directly would apply the
                // matrix to values already scaled by alpha, which is not the
                // colour that was composited. Undo the premultiplication for the
                // conversion and put it back afterwards.
                let (r, g, b) = if premultiplied {
                    unpremultiply(r, g, b, p[3])
                } else {
                    (r, g, b)
                };
                let out = to_display.convert_linear([
                    linear[r as usize],
                    linear[g as usize],
                    linear[b as usize],
                ]);
                let quantise = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
                let (r, g, b) = (quantise(out[0]), quantise(out[1]), quantise(out[2]));
                if premultiplied {
                    (scale(r, p[3]), scale(g, p[3]), scale(b, p[3]))
                } else {
                    (r, g, b)
                }
            };
            pixels.push(if premultiplied {
                Color32::from_rgba_premultiplied(r, g, b, p[3])
            } else {
                Color32::from_rgba_unmultiplied(r, g, b, p[3])
            });
        }
    }
    egui::ColorImage {
        size: [w, h],
        pixels,
        source_size: egui::vec2(w as f32, h as f32),
    }
}

/// Recovers straight channels from premultiplied ones.
fn unpremultiply(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8) {
    if a == 0 {
        return (0, 0, 0);
    }
    let recover = |c: u8| ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8;
    (recover(r), recover(g), recover(b))
}

/// Scales one channel by alpha, rounding half up.
fn scale(c: u8, a: u8) -> u8 {
    ((u32::from(c) * u32::from(a) + 127) / 255) as u8
}

/// Where the document ended up on screen, and what the pointer did.
#[derive(Debug, Clone, Copy)]
pub struct CanvasView {
    /// The complete rendered Scene rectangle on screen.
    pub image: Rect,
    /// The untouched capture subject rectangle within the Scene.
    pub subject: Rect,
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
            self.subject.width() / self.content.size.width as f32
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
    interactive: bool,
) -> CanvasView {
    let palette = surface.palette();
    let painter = ui.painter_at(area);
    checkerboard(&painter, area, palette);

    let content = if state.crop_mode() {
        state.document().logical_bounds()
    } else {
        state.document().content_bounds()
    };
    let output = preview_output_size(state);
    let output_bounds = LogicalRect::new(LogicalPoint::new(0.0, 0.0), output);
    let fit_area = area.shrink(Space::LG);
    let image = if state.is_fit_zoom() {
        super::fit(output_bounds, fit_area, 1.0, state.pan())
    } else {
        super::fit_absolute(output_bounds, fit_area, state.zoom(), state.pan())
    };

    let max_texture_side = ui.ctx().input(|input| input.max_texture_side);
    let max_texture_side = u32::try_from(max_texture_side).unwrap_or(u32::MAX);
    let target_px = preview_width(
        image,
        ui.ctx().pixels_per_point(),
        output,
        state.document().logical_size(),
        max_texture_side,
    );
    let rendered = target_px.and_then(|target_px| preview.texture(ui.ctx(), state, target_px));
    let subject = rendered
        .and_then(|(_, subject)| subject)
        .map_or(image, |subject| {
            Rect::from_min_max(
                image.min + subject.min.to_vec2() * image.size(),
                image.min + subject.max.to_vec2() * image.size(),
            )
        });
    let view = CanvasView {
        image,
        subject,
        area,
        content,
        hovered: false,
    };
    // Hand the layout back to the state so screen-space hit tolerances survive
    // zoom. Done before `gestures` so the very first press of a frame is judged
    // against the scale that frame is actually drawn at.
    state.set_view_scale(f64::from(view.scale()));

    if let Some((texture, _)) = rendered {
        painter.image(
            texture,
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

    let response = ui.interact(
        area,
        ui.id().with("editor-canvas"),
        if interactive {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        },
    );
    let view = CanvasView {
        hovered: response.hovered(),
        ..view
    };
    if interactive {
        gestures(ui, state, &response, &view);
    }

    let chrome = ui.painter_at(area);
    draw_crop_scrim(&chrome, state, &view, palette);
    draw_selection(&chrome, state, &view, palette);
    // Taken before drawing, so it is consumed whether or not there is still a
    // caret to draw: an undo that removed the label outright must not leave the
    // instruction queued for the next label the user makes.
    let interrupt = state.take_ime_interrupt();
    draw_caret(ui, &chrome, state, &view, palette, interrupt);
    if interactive {
        cursor(ui, state, &response);
    }
    view
}

fn preview_output_size(state: &EditorState) -> LogicalSize {
    if state.crop_mode() && state.document().crop().is_some() {
        let mut document = state.document().clone();
        document
            .set_crop(None)
            .expect("a valid document remains valid when its crop is removed");
        document.output_logical_size()
    } else {
        state.document().output_logical_size()
    }
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
    interrupt: bool,
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
    let top = super::to_screen(LogicalPoint::new(x, y), view.subject, view.content);
    let bottom = super::to_screen(LogicalPoint::new(x, y + size), view.subject, view.content);
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
            // Set only when the history moved the text under the IME. The IME
            // is in another process and would otherwise keep composing against
            // a string that no longer exists, committing glyphs from an edit the
            // user already took back.
            should_interrupt_composition: interrupt,
        });
    });
}

/// The width, in pixels, the preview should be rendered at.
fn preview_width(
    image: Rect,
    ppp: f32,
    content: LogicalSize,
    source: LogicalSize,
    max_texture_side: u32,
) -> Option<u32> {
    if content.is_empty() || source.is_empty() || max_texture_side == 0 {
        return None;
    }
    let want = (image.width() * ppp).ceil().max(1.0) as u32;
    let texture_cap = if content.height > content.width {
        (f64::from(max_texture_side) * content.width / content.height)
            .floor()
            .max(0.0) as u32
    } else {
        max_texture_side
    };
    let source_area = source.width * source.height;
    let composite_cap = (content.width * (MAX_PREVIEW_COMPOSITE_PIXELS / source_area).sqrt())
        .floor()
        .max(0.0) as u32;
    let cap = MAX_PREVIEW_PX
        .min(max_texture_side)
        .min(texture_cap)
        .min(composite_cap);
    if cap == 0 {
        return None;
    }
    // Quantise so that a one-pixel window resize does not re-render: without
    // this, dragging the window edge re-composites the whole capture on every
    // frame of the drag.
    let quantised = want.div_ceil(64).saturating_mul(64);
    Some(quantised.min(cap))
}

/// Translates pointer events into state-machine calls.
fn gestures(ui: &Ui, state: &mut EditorState, response: &egui::Response, view: &CanvasView) {
    let modifiers = EditorModifiers::read(ui);
    let Some(screen) = response
        .interact_pointer_pos()
        .or(response.hover_pos())
        .or_else(|| ui.ctx().input(|input| input.pointer.latest_pos()))
    else {
        if response.drag_stopped() || response.drag_stopped_by(egui::PointerButton::Middle) {
            state.pointer_released();
        }
        return;
    };
    let point = to_document(screen, view.subject, view.content);
    let (middle_pressed, middle_down, middle_released) = ui.ctx().input(|input| {
        (
            input.pointer.button_pressed(egui::PointerButton::Middle),
            input.pointer.button_down(egui::PointerButton::Middle),
            input.pointer.button_released(egui::PointerButton::Middle),
        )
    });
    if middle_pressed && view.area.contains(screen) {
        state.begin_pan((screen.x, screen.y));
    }
    if state.is_panning() && (middle_down || middle_released) {
        state.pan_to((screen.x, screen.y));
        if middle_released {
            state.pointer_released();
        }
        return;
    }
    let pan_gesture = modifiers.pan;

    if response.drag_started() {
        let press_screen = screen - response.drag_delta();
        let press = to_document(press_screen, view.subject, view.content);
        if pan_gesture {
            state.begin_pan((press_screen.x, press_screen.y));
            state.pan_to((screen.x, screen.y));
        } else {
            state.pointer_pressed(press);
            state.pointer_dragged_with_snap(
                point,
                modifiers.constrain,
                modifiers.disable_crop_snap,
            );
        }
    } else if response.dragged() {
        if pan_gesture {
            state.pan_to((screen.x, screen.y));
        } else {
            state.pointer_dragged_with_snap(
                point,
                modifiers.constrain,
                modifiers.disable_crop_snap,
            );
        }
    } else if response.drag_stopped() {
        if pan_gesture {
            state.pan_to((screen.x, screen.y));
        } else {
            state.pointer_dragged_with_snap(
                point,
                modifiers.constrain,
                modifiers.disable_crop_snap,
            );
        }
        state.pointer_released();
    } else if response.clicked() && !modifiers.pan {
        // A click with no drag never reaches drag_started, so place-tools and
        // click-to-select would do nothing at all without this.
        state.pointer_pressed(point);
        state.pointer_released();
    }

    if response.hovered() {
        let (zoom_delta, scroll, multi_touch) = ui.ctx().input(|input| {
            (
                input.zoom_delta(),
                input.smooth_scroll_delta(),
                input.multi_touch().is_some(),
            )
        });
        if (zoom_delta - 1.0).abs() > f32::EPSILON {
            let anchor = screen - view.area.center();
            state.zoom_about(state.effective_zoom() * zoom_delta, (anchor.x, anchor.y));
        }
        if scroll != egui::Vec2::ZERO
            && !state.is_fit_zoom()
            && ((zoom_delta - 1.0).abs() <= f32::EPSILON || multi_touch)
        {
            let pan = state.pan();
            state.set_pan((pan.0 + scroll.x, pan.1 + scroll.y));
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
        return;
    };
    let keep = rect_to_screen(rect, view.subject, view.content);
    if rect != state.document().logical_bounds() {
        let scrim = Color32::from_black_alpha(120);
        for band in [
            Rect::from_min_max(view.subject.min, pos2(view.subject.right(), keep.top())),
            Rect::from_min_max(pos2(view.subject.left(), keep.bottom()), view.subject.max),
            Rect::from_min_max(pos2(view.subject.left(), keep.top()), keep.left_bottom()),
            Rect::from_min_max(keep.right_top(), pos2(view.subject.right(), keep.bottom())),
        ] {
            if band.is_positive() {
                painter.rect_filled(band, corner(0.0), scrim);
            }
        }
    }
    painter.rect_stroke(
        keep.expand(1.0),
        corner(0.0),
        Stroke::new(3.0, Color32::from_black_alpha(130)),
        StrokeKind::Outside,
    );
    painter.rect_stroke(
        keep,
        corner(0.0),
        Stroke::new(1.5, Color32::WHITE),
        StrokeKind::Outside,
    );
    thirds(painter, keep);
    for handle in Handle::ALL {
        let at = super::to_screen(handle.position(&rect), view.subject, view.content);
        let radius = HANDLE_RADIUS as f32;
        let handle_rect = Rect::from_center_size(at, vec2(radius * 2.0, radius * 2.0));
        painter.rect_filled(
            handle_rect.expand(1.5),
            corner(2.0),
            Color32::from_black_alpha(150),
        );
        painter.rect_filled(handle_rect, corner(1.5), Color32::WHITE);
        painter.rect_stroke(
            handle_rect,
            corner(1.5),
            Stroke::new(1.0, palette.accent),
            StrokeKind::Inside,
        );
    }
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

/// Draws the selection chrome.
fn draw_selection(
    painter: &egui::Painter,
    state: &EditorState,
    view: &CanvasView,
    palette: &Palette,
) {
    let Some(bounds) = state.selection_bounds() else {
        return;
    };
    let handles = state.selection_handles();
    let arrow = handles
        .first()
        .is_some_and(|(handle, _)| matches!(handle, Handle::ArrowStart | Handle::ArrowEnd));
    if arrow {
        let r = HANDLE_RADIUS as f32;
        for (_, position) in handles {
            let at = super::to_screen(position, view.subject, view.content);
            // Dark under-ring + white keyline + accent centre: three contrasts,
            // so the endpoint stays visible over any captured pixel.
            painter.circle_filled(at, r + 2.0, Color32::from_black_alpha(100));
            painter.circle_filled(at, r + 1.0, Color32::WHITE);
            painter.circle_filled(at, r - 0.5, palette.accent);
            painter.circle_stroke(at, r - 0.5, Stroke::new(1.0, palette.accent_press));
        }
        if let Some(position) = state.arrow_bend_handle() {
            let at = super::to_screen(position, view.subject, view.content);
            let near = painter
                .ctx()
                .pointer_hover_pos()
                .is_some_and(|pointer| pointer.distance(at) <= 18.0);
            if state.arrow_style() == ArrowStyle::Curved || state.is_dragging_arrow_bend() || near {
                let diamond = |radius: f32| {
                    vec![
                        at + vec2(0.0, -radius),
                        at + vec2(radius, 0.0),
                        at + vec2(0.0, radius),
                        at + vec2(-radius, 0.0),
                    ]
                };
                painter.add(egui::Shape::convex_polygon(
                    diamond(r + 2.0),
                    Color32::from_black_alpha(110),
                    Stroke::NONE,
                ));
                painter.add(egui::Shape::convex_polygon(
                    diamond(r + 1.0),
                    Color32::WHITE,
                    Stroke::NONE,
                ));
                painter.add(egui::Shape::convex_polygon(
                    diamond(r - 0.5),
                    palette.accent,
                    Stroke::new(1.0, palette.accent_press),
                ));
            }
        }
        return;
    }
    let rect = rect_to_screen(bounds, view.subject, view.content).expand(2.0);
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
    for (handle, position) in handles {
        let at = super::to_screen(position, view.subject, view.content);
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

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_annotate::{Annotation, Beautification, Document, RedactStyle, Style};
    use scrozz_core::{Capture, CaptureTarget, Frame, PhysicalSize, Provenance};

    #[test]
    fn a_tall_crop_fits_both_texture_edges() {
        let content = LogicalSize::new(4.0, 1_080.0);
        let width = preview_width(
            Rect::from_min_size(pos2(0.0, 0.0), vec2(300.0, 600.0)),
            2.0,
            content,
            content,
            4_096,
        )
        .expect("a feasible preview");
        let height = (f64::from(width) * content.height / content.width).round() as u32;
        assert!(width <= 4_096);
        assert!(height <= 4_096);
    }

    #[test]
    fn a_tiny_crop_cannot_request_a_gigantic_source_composite() {
        let width = preview_width(
            Rect::from_min_size(pos2(0.0, 0.0), vec2(1_000.0, 1_000.0)),
            2.0,
            LogicalSize::new(10.0, 10.0),
            LogicalSize::new(1_000.0, 1_000.0),
            16_384,
        )
        .expect("a feasible preview");
        let scale = f64::from(width) / 10.0;
        let source_pixels = 1_000.0 * scale * 1_000.0 * scale;
        assert!(source_pixels <= MAX_PREVIEW_COMPOSITE_PIXELS);
    }

    #[test]
    fn an_impossible_preview_budget_is_not_rounded_up() {
        assert_eq!(
            preview_width(
                Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0)),
                1.0,
                LogicalSize::new(0.05, 300.0),
                LogicalSize::new(400.0, 300.0),
                4_096,
            ),
            None
        );
    }

    #[test]
    fn full_source_crop_preview_still_destroys_redacted_pixels() {
        let size = LogicalSize::new(8.0, 8.0);
        let capture = Capture {
            frame: Frame {
                data: vec![255; 8 * 8 * 4],
                size: PhysicalSize::new(8.0, 8.0),
                stride: 8 * 4,
                format: PixelFormat::Rgba8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::IDENTITY,
            },
            provenance: Provenance::Region,
            target: CaptureTarget::Region(LogicalRect::new(LogicalPoint::new(0.0, 0.0), size)),
        };
        let mut document = Document::new(capture);
        document.add(
            Annotation::Redact {
                area: LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(4.0, 4.0)),
                style: RedactStyle::Solid,
            },
            Style::redaction(),
        );
        document
            .set_crop(Some(LogicalRect::new(
                LogicalPoint::new(4.0, 0.0),
                LogicalSize::new(4.0, 8.0),
            )))
            .unwrap();
        let mut state = EditorState::new(document);
        state.set_tool(Tool::Crop);

        let preview = render(&state, 8, true).unwrap();
        assert_eq!(
            preview.image.pixels[1 + preview.image.size[0]],
            Color32::BLACK,
            "showing the full source in crop mode exposed pixels beneath a redaction"
        );
        assert_eq!(state.document().crop().unwrap().origin.x, 4.0);
    }

    #[test]
    fn scene_preview_keeps_canvas_aspect_and_reports_subject_mapping() {
        let size = LogicalSize::new(8.0, 8.0);
        let capture = Capture {
            frame: Frame {
                data: vec![255; 8 * 8 * 4],
                size: PhysicalSize::new(8.0, 8.0),
                stride: 8 * 4,
                format: PixelFormat::Rgba8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::IDENTITY,
            },
            provenance: Provenance::Region,
            target: CaptureTarget::Region(LogicalRect::new(LogicalPoint::new(0.0, 0.0), size)),
        };
        let mut document = Document::new(capture);
        document
            .set_scene(Some(Beautification {
                output_size: Some(scrozz_annotate::ExactOutputSize {
                    width: 16,
                    height: 8,
                }),
                background: scrozz_annotate::Background::Transparent,
                ..Beautification::default()
            }))
            .unwrap();
        let state = EditorState::new(document);

        let preview = render(&state, 160, false).unwrap();
        assert_eq!(preview.image.size, [160, 80]);
        let subject = preview.subject.expect("Scene subject mapping");
        assert_eq!(subject.min, pos2(0.25, 0.0));
        assert_eq!(subject.max, pos2(0.75, 1.0));

        let image = Rect::from_min_size(pos2(20.0, 30.0), vec2(200.0, 100.0));
        let subject = Rect::from_min_max(
            image.min + subject.min.to_vec2() * image.size(),
            image.min + subject.max.to_vec2() * image.size(),
        );
        assert_eq!(
            to_document(subject.min, subject, state.document().content_bounds()),
            LogicalPoint::new(0.0, 0.0)
        );
        assert_eq!(
            to_document(subject.max, subject, state.document().content_bounds()),
            LogicalPoint::new(8.0, 8.0)
        );
    }
}
