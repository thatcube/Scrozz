//! Painting one capture card.
//!
//! [`stack`](crate::stack) decides where every card is and how far through each
//! animation it has travelled. This module turns one of its [`CardFrame`]s into
//! pixels: the capture itself, a caption, and the chrome that appears on hover.
//!
//! # What a card is made of
//!
//! At rest a card is the capture and a caption strip, and nothing else (D12).
//! On hover a scrim fades in carrying two prominent pills — **Copy** and
//! **Save**, the two things people actually do with a screenshot — and four
//! quiet corner icons for pin, close, annotate and upload (D21). The card body
//! *is* the drag handle; there is no separate grab affordance.
//!
//! # Motion applies to the card, not to its buttons
//!
//! The reveal animates because the chrome is an object arriving (D19). The
//! buttons inside it do not animate at all: hover and press are instant state
//! changes, which is why [`paint`](crate::paint)'s controls contain no easing
//! and why this module never wraps one in a fade. Instant feedback reads as
//! faster than an eased one, and a control is not an object.
//!
//! # D9: a window capture is never composited onto
//!
//! A window capture already carries the compositor's own corner radius, its
//! shadow, and the transparency between the two. Painting a synthetic radius,
//! shadow, padding or backing plate on top of that produces a double corner and
//! a double shadow, and it bakes the host's idea of a window into a picture the
//! user will paste somewhere else.
//!
//! [`CardChrome::for_provenance`] is the single place that decision is made, and
//! [`CardChrome::composites`] reports it so a test can assert it directly rather
//! than inferring it from pixels. Every overlay this module draws — the hover
//! scrim, the caption scrim, the placeholder — takes its rounding from
//! [`CardChrome::overlay_radius`], which is by construction the *same* value the
//! capture is drawn with. That is not a stylistic preference: a scrim drawn with
//! square corners over a rounded thumbnail squares the bottom of the card, which
//! is exactly the defect that shipped once before and which nobody caught until
//! a human looked at it.

use egui::epaint::{Mesh, Vertex};
use egui::{
    Color32, Id, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, WidgetInfo, WidgetType,
    pos2, vec2,
};
use scrozz_core::Provenance;

use crate::icons::Icon;
use crate::motion::fade;
use crate::paint::{self, ControlState, Reveal, Surface};
use crate::stack::{CardFrame, CardId, MAX_LEAN};
use crate::theme::{Radius, Space, Text, corner};

/// Height of a revealed pill button.
const PILL_H: f32 = 30.0;
/// Side of a revealed corner icon button.
const ICON_BTN: f32 = 28.0;
/// Inset of revealed chrome from the capture's edge.
const CHROME_INSET: f32 = Space::SM;
/// Peak alpha of the hover scrim.
const HOVER_SCRIM: f32 = 132.0;
/// Peak alpha of the caption scrim, matching [`paint::caption_strip`].
const CAPTION_SCRIM: f32 = 205.0;
/// Height of the caption scrim, matching [`paint::caption_strip`].
const CAPTION_H: f32 = 58.0;

// ---------------------------------------------------------------------------
// Chrome
// ---------------------------------------------------------------------------

/// The geometry a card is allowed to add around a capture.
///
/// Derived from [`Provenance`] alone, so the D9 decision is one pure function
/// with one input and no ambient state. Constructing it is free; asserting on it
/// is exact.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CardChrome {
    /// Corner radius applied to the capture, in points. Zero when the capture
    /// carries its own corners.
    pub thumb_radius: f32,
    /// Radius for *anything* drawn over the capture — scrims, placeholders,
    /// hover washes.
    ///
    /// Always equal to [`Self::thumb_radius`]. It is a separate field so the
    /// invariant is nameable and testable rather than implicit in a call site.
    pub overlay_radius: f32,
    /// Inset of the capture inside the card rectangle, in points.
    pub padding: f32,
    /// Whether a synthetic backing plate is drawn behind the capture.
    pub plate: bool,
    /// Whether a synthetic drop shadow is drawn under the card.
    pub shadow: bool,
}

impl CardChrome {
    /// The card's own outer radius.
    pub const OUTER_RADIUS: f32 = Radius::CARD;

    /// Padding between the card edge and the capture.
    ///
    /// Chosen so the concentric-radius rule holds exactly:
    /// `inner = outer − padding`, i.e. [`Radius::THUMB`] sits inside
    /// [`Radius::CARD`] with no optical pinch at the corners.
    pub const PADDING: f32 = Radius::CARD - Radius::THUMB;

    /// The chrome permitted for a capture of this provenance.
    #[must_use]
    pub fn for_provenance(provenance: Provenance) -> Self {
        if provenance.forbids_compositing() {
            // A window capture is delivered with its own radius, its own shadow
            // and the alpha between them. Everything here is off, and stays off
            // in every interaction state including drag (D9).
            Self {
                thumb_radius: 0.0,
                overlay_radius: 0.0,
                padding: 0.0,
                plate: false,
                shadow: false,
            }
        } else {
            Self {
                thumb_radius: Radius::THUMB,
                overlay_radius: Radius::THUMB,
                padding: Self::PADDING,
                plate: true,
                shadow: true,
            }
        }
    }

    /// Whether this chrome adds any synthetic geometry to the capture at all.
    ///
    /// The one assertion a D9 test needs.
    #[must_use]
    pub fn composites(self) -> bool {
        self.plate || self.shadow || self.padding > 0.0 || self.thumb_radius > 0.0
    }

    /// Whether the overlay rounding matches the capture's rounding.
    ///
    /// Must always be true. A scrim, wash or placeholder drawn at a different
    /// radius from the thing beneath it changes the silhouette of the card,
    /// which is the D9 defect in its original form.
    #[must_use]
    pub fn overlays_match(self) -> bool {
        (self.overlay_radius - self.thumb_radius).abs() < f32::EPSILON
    }

    /// Whether `inner = outer − padding` holds.
    #[must_use]
    pub fn is_concentric(self) -> bool {
        (Self::OUTER_RADIUS - self.padding - self.thumb_radius).abs() < 0.001
    }

    /// Where the capture is drawn inside a card rectangle.
    #[must_use]
    pub fn capture_rect(self, card: Rect) -> Rect {
        card.shrink(self.padding)
    }
}

// ---------------------------------------------------------------------------
// Content and results
// ---------------------------------------------------------------------------

/// Everything the painter needs to know about one capture.
///
/// A borrowed view, not an owner: the overlay keeps the textures and strings, and
/// a still render can synthesise one without allocating a capture.
#[derive(Clone, Copy, Debug)]
pub struct CardContent<'a> {
    /// File name, shown at the left of the caption.
    pub name: &'a str,
    /// Capture size in pixels, shown at the right of the caption.
    pub source_px: (u32, u32),
    /// Where the pixels came from. Decides the chrome (D9).
    pub provenance: Provenance,
    /// The uploaded thumbnail, if it has been uploaded yet.
    pub texture: Option<egui::TextureId>,
    /// Whether the owning history model currently protects this capture.
    pub pinned: bool,
}

impl<'a> CardContent<'a> {
    /// A capture with no thumbnail uploaded yet.
    #[must_use]
    pub fn new(name: &'a str, source_px: (u32, u32), provenance: Provenance) -> Self {
        Self {
            name,
            source_px,
            provenance,
            texture: None,
            pinned: false,
        }
    }

    /// The same content with a thumbnail attached.
    #[must_use]
    pub fn with_texture(mut self, texture: egui::TextureId) -> Self {
        self.texture = Some(texture);
        self
    }

    /// Marks the capture as pinned for its control label.
    #[must_use]
    pub const fn with_pinned(mut self, pinned: bool) -> Self {
        self.pinned = pinned;
        self
    }

    /// The right-hand caption detail, e.g. `2560 × 1440`.
    #[must_use]
    pub fn dimensions(&self) -> String {
        format!("{} × {}", self.source_px.0, self.source_px.1)
    }
}

/// What the user asked for by pressing something on a card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CardAction {
    /// Copy the capture to the clipboard. Primary (D21).
    Copy,
    /// Save the capture to disk. Primary (D21).
    Save,
    /// Open the annotation editor.
    Annotate,
    /// Upload and produce a link.
    Upload,
    /// Pin the card so the stack will not retire it.
    Pin,
    /// Dismiss the card.
    Close,
}

impl CardAction {
    /// A stable slug, for logging and accessibility labels.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Save => "save",
            Self::Annotate => "annotate",
            Self::Upload => "upload",
            Self::Pin => "pin",
            Self::Close => "close",
        }
    }

    /// The human label, used as the pill text and the widget's accessible name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::Save => "Save",
            Self::Annotate => "Annotate",
            Self::Upload => "Upload",
            Self::Pin => "Pin",
            Self::Close => "Close",
        }
    }

    /// The icon that stands for it.
    #[must_use]
    pub const fn icon(self) -> Icon {
        match self {
            Self::Copy => Icon::Copy,
            Self::Save => Icon::DeviceFloppy,
            Self::Annotate => Icon::Pencil,
            Self::Upload => Icon::CloudUpload,
            Self::Pin => Icon::Pin,
            Self::Close => Icon::X,
        }
    }
}

/// The outcome of painting one card.
#[derive(Clone, Debug)]
pub struct CardResponse {
    /// The card body, which is also the drag handle (D21).
    pub body: Response,
    /// The button the user pressed this frame, if any.
    pub action: Option<CardAction>,
    /// The chrome that was applied, so a caller — or a test — can inspect the
    /// D9 decision without re-deriving it.
    pub chrome: CardChrome,
    /// The rectangle the card actually occupies on screen, for the overlay's
    /// click-through hit test.
    pub hit: Rect,
}

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

/// A stable widget id for one control on one card.
///
/// Derived from the card's identity, never from its rectangle: cards move
/// continuously while the stack animates, and an id that moves with them drops
/// focus and breaks press tracking mid-gesture.
#[must_use]
pub fn control_id(card: CardId, action: CardAction) -> Id {
    Id::new(("scrozz.card", card, action.slug()))
}

/// The id of a card's body, which is the drag handle.
#[must_use]
pub fn body_id(card: CardId) -> Id {
    Id::new(("scrozz.card.body", card))
}

fn accessible_name(content: &CardContent<'_>) -> String {
    format!(
        "Capture {}, {}. Activate to annotate. Drag right or up to share.",
        content.name,
        content.dimensions()
    )
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

/// Draw one card and return what the user did to it.
///
/// `frame` comes straight from [`CaptureStack::frame`](crate::stack::CaptureStack::frame)
/// and carries position, opacity, hover reveal, lift and lean. Nothing here
/// computes motion; it only draws what the stack decided.
pub fn draw_card(
    ui: &mut Ui,
    surface: &Surface<'_>,
    frame: &CardFrame,
    content: &CardContent<'_>,
) -> CardResponse {
    let chrome = CardChrome::for_provenance(content.provenance);
    let rect = frame.rect;
    let alpha = frame.alpha.clamp(0.0, 1.0);
    let palette = surface.palette();

    // The body is sensed before anything is painted so the response is available
    // to the chrome, and so the card sits *under* its own buttons in the layer
    // order rather than swallowing their clicks.
    let body = ui.interact(rect, body_id(frame.id), Sense::click_and_drag());
    let accessible_name = accessible_name(content);
    body.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, accessible_name.clone()));

    if alpha <= 0.001 {
        return CardResponse {
            body,
            action: None,
            chrome,
            hit: rect,
        };
    }

    // How square-on the card is. Text and controls cannot rotate in egui, so
    // they fade as the card leans rather than tilting with it.
    let flat = (1.0 - frame.angle.abs() / MAX_LEAN).clamp(0.0, 1.0);
    let angle = frame.angle;

    if chrome.shadow {
        // Resting cards are already off the desktop; lift deepens it.
        let lift = frame.lift.mul_add(0.55, 0.45) * alpha;
        paint::soft_shadow(ui.painter(), rect, CardChrome::OUTER_RADIUS, palette, lift);
    }

    if chrome.plate {
        let painter = ui.painter();
        let fill = fade(palette.card_fill_raised, alpha);
        if angle.abs() > f32::EPSILON {
            paint::fill_rot_round_rect(
                painter,
                rect,
                CardChrome::OUTER_RADIUS,
                rect.center(),
                angle,
                fill,
            );
        } else {
            let cr = corner(CardChrome::OUTER_RADIUS);
            painter.rect_filled(rect, cr, fill);
            painter.rect_stroke(
                rect,
                cr,
                Stroke::new(1.0, fade(palette.hairline, alpha)),
                StrokeKind::Inside,
            );
        }
    }

    let capture = chrome.capture_rect(rect);
    draw_capture(ui, surface, content, chrome, capture, angle, alpha);

    // The caption belongs to the resting card; the hover chrome replaces it, so
    // they cross-fade rather than colliding at the bottom edge.
    let reveal = frame.reveal.clamp(0.0, 1.0) * flat;
    let caption_alpha = alpha * (1.0 - reveal) * flat;
    if caption_alpha > 0.004 {
        draw_caption(
            ui,
            surface,
            capture,
            chrome,
            content,
            caption_alpha,
            reveal,
            angle,
        );
    }

    let action = draw_chrome(
        ui,
        surface,
        frame,
        chrome,
        capture,
        alpha * reveal,
        content.pinned,
    );

    CardResponse {
        body,
        action,
        chrome,
        hit: rect,
    }
}

/// Draw the capture itself, or a neutral placeholder while it uploads.
fn draw_capture(
    ui: &mut Ui,
    surface: &Surface<'_>,
    content: &CardContent<'_>,
    chrome: CardChrome,
    capture: Rect,
    angle: f32,
    alpha: f32,
) {
    let painter = ui.painter();
    let palette = surface.palette();

    let Some(texture) = content.texture else {
        // No pixels yet, so there is nothing to composite *onto*: a neutral
        // holding fill is not a violation. It still takes the capture's own
        // rounding, so the card's silhouette never changes when the thumbnail
        // arrives.
        let fill = fade(palette.card_fill, alpha * 0.9);
        if angle.abs() > f32::EPSILON {
            paint::fill_rot_round_rect(
                painter,
                capture,
                chrome.overlay_radius,
                capture.center(),
                angle,
                fill,
            );
        } else {
            painter.rect_filled(capture, corner(chrome.overlay_radius), fill);
        }
        return;
    };

    // Contain, never cover: cropping a thumbnail tells the user they captured
    // something they did not.
    let fitted = fit(capture, content.source_px);
    let tint = Color32::WHITE.gamma_multiply(alpha);
    textured_round_rect(painter, texture, fitted, chrome.thumb_radius, angle, tint);

    if chrome.plate {
        // A hairline only where we already own the geometry. Never around a
        // window capture, whose edge is its own (D9).
        painter.rect_stroke(
            fitted,
            corner(chrome.thumb_radius),
            Stroke::new(1.0, fade(palette.thumb_border, alpha)),
            StrokeKind::Inside,
        );
    }
}

/// Name on the left, dimensions on the right, over a scrim that takes the
/// capture's rounding.
#[allow(clippy::too_many_arguments)]
fn draw_caption(
    ui: &mut Ui,
    surface: &Surface<'_>,
    capture: Rect,
    chrome: CardChrome,
    content: &CardContent<'_>,
    alpha: f32,
    _reveal: f32,
    angle: f32,
) {
    if angle.abs() > f32::EPSILON {
        // Rotated text is not expressible in egui at all, so a leaning card
        // simply has no caption. It is mid-gesture; the label is not what the
        // user is looking at.
        return;
    }
    let painter = ui.painter();

    // `bottom_scrim` rounds its *bottom* corners to the radius it is given, so
    // passing the capture's radius is what keeps the card's silhouette intact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let peak = (CAPTION_SCRIM * alpha).round().clamp(0.0, 255.0) as u8;
    if peak > 0 {
        paint::bottom_scrim(painter, capture, CAPTION_H, chrome.overlay_radius, peak);
    }

    const SIDE_INSET: f32 = 13.0;
    const LABEL_GAP: f32 = 8.0;
    const MIN_NAME_WIDTH: f32 = 56.0;

    let cy = capture.bottom() - 15.0;
    let left = capture.left() + SIDE_INSET;
    let right = capture.right() - SIDE_INSET;
    let available = (right - left).max(0.0);
    let name_color = fade(Color32::from_rgb(255, 255, 255), alpha * 0.925);
    let detail_color = fade(Color32::from_rgb(255, 255, 255), alpha * 0.59);
    let detail = painter.layout_no_wrap(
        content.dimensions(),
        surface.font(Text::Caption),
        detail_color,
    );
    let show_detail = available >= MIN_NAME_WIDTH + LABEL_GAP + detail.size().x;
    let name_width = if show_detail {
        available - LABEL_GAP - detail.size().x
    } else {
        available
    };
    let mut name_job = egui::text::LayoutJob::simple(
        content.name.to_owned(),
        surface.font(Text::Label),
        name_color,
        name_width,
    );
    name_job.wrap.max_rows = 1;
    name_job.wrap.break_anywhere = true;
    name_job.wrap.overflow_character = Some('…');
    let name = painter.layout_job(name_job);
    painter.galley(pos2(left, cy - name.size().y * 0.5), name, name_color);

    if show_detail {
        painter.galley(
            pos2(right - detail.size().x, cy - detail.size().y * 0.5),
            detail,
            detail_color,
        );
    }
}

/// The hover chrome: a scrim, two primary pills, four quiet corner icons.
///
/// Returns the action pressed this frame. The controls themselves are drawn by
/// [`paint`], which contains no animation — the fade lives entirely in the
/// [`Reveal`] passed through them (D19).
fn draw_chrome(
    ui: &mut Ui,
    surface: &Surface<'_>,
    frame: &CardFrame,
    chrome: CardChrome,
    capture: Rect,
    opacity: f32,
    pinned: bool,
) -> Option<CardAction> {
    if opacity <= 0.004 {
        return None;
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scrim = (HOVER_SCRIM * opacity).round().clamp(0.0, 255.0) as u8;
    if scrim > 0 {
        // Same rounding as the capture beneath it. This single argument is the
        // whole of D9 at this call site.
        ui.painter().rect_filled(
            capture,
            corner(chrome.overlay_radius),
            Color32::from_black_alpha(scrim),
        );
    }

    let inner = capture.shrink(CHROME_INSET);
    if inner.width() < ICON_BTN * 2.0 || inner.height() < ICON_BTN * 2.0 {
        return None;
    }

    let lift = Reveal::new(opacity, vec2(0.0, (1.0 - opacity) * 6.0));
    let settle = Reveal::new(opacity, vec2(0.0, (1.0 - opacity) * -3.0));
    let mut pressed = None;

    // Primary: Copy and Save, side by side, centred.
    let gap = Space::SM;
    let pill_w = ((inner.width() - gap) * 0.5).min(112.0);
    let pill_y = inner.center().y - PILL_H * 0.5;
    let total = pill_w.mul_add(2.0, gap);
    let pill_x = inner.center().x - total * 0.5;
    for (i, action) in [CardAction::Copy, CardAction::Save].into_iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let x = (pill_w + gap).mul_add(i as f32, pill_x);
        let r = Rect::from_min_size(pos2(x, pill_y), vec2(pill_w, PILL_H));
        let resp = paint::pill_button(
            ui,
            surface,
            r,
            control_id(frame.id, action),
            action.icon(),
            action.label(),
            action == CardAction::Copy,
            lift,
        );
        if resp.clicked() {
            pressed = Some(action);
        }
    }

    // Secondary: four corners. Pin and Close on top, Annotate and Upload below,
    // so the destructive one is farthest from the primary pair.
    let size = vec2(ICON_BTN, ICON_BTN);
    let corners = [
        (CardAction::Pin, inner.left_top()),
        (
            CardAction::Close,
            pos2(inner.right() - ICON_BTN, inner.top()),
        ),
        (
            CardAction::Annotate,
            pos2(inner.left(), inner.bottom() - ICON_BTN),
        ),
        (
            CardAction::Upload,
            pos2(inner.right() - ICON_BTN, inner.bottom() - ICON_BTN),
        ),
    ];
    for (action, origin) in corners {
        let r = Rect::from_min_size(origin, size);
        let resp = paint::icon_button(
            ui,
            surface,
            r,
            control_id(frame.id, action),
            action.icon(),
            if action == CardAction::Pin && pinned {
                "Unpin"
            } else {
                action.label()
            },
            ControlState::new(),
            settle,
        );
        if resp.clicked() {
            pressed = Some(action);
        }
    }

    pressed
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

/// Fit `source_px` inside `bounds`, preserving aspect and centring.
///
/// Degenerate sizes fall back to the whole rectangle rather than producing a
/// zero-area or NaN rectangle.
#[must_use]
pub fn fit(bounds: Rect, source_px: (u32, u32)) -> Rect {
    let (w, h) = source_px;
    if w == 0 || h == 0 || bounds.width() <= 0.0 || bounds.height() <= 0.0 {
        return bounds;
    }
    #[allow(clippy::cast_precision_loss)]
    let aspect = w as f32 / h as f32;
    let by_width = vec2(bounds.width(), bounds.width() / aspect);
    let size = if by_width.y <= bounds.height() {
        by_width
    } else {
        vec2(bounds.height() * aspect, bounds.height())
    };
    Rect::from_center_size(bounds.center(), size)
}

/// Draw a texture into a rounded — and optionally rotated — rectangle.
///
/// egui has no rounded image primitive and no rotation primitive, so this builds
/// the mesh: a triangle fan over [`paint::rounded_poly`], with UVs taken from the
/// *unrotated* geometry so the image rotates with the shape instead of sliding
/// under it. A `radius` of zero degenerates to a plain quad, which is precisely
/// what a window capture wants.
pub fn textured_round_rect(
    painter: &egui::Painter,
    texture: egui::TextureId,
    rect: Rect,
    radius: f32,
    radians: f32,
    tint: Color32,
) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let mut pts = paint::rounded_poly(rect, radius);
    if pts.len() < 3 {
        return;
    }
    let uv_of = |p: egui::Pos2| {
        pos2(
            (p.x - rect.left()) / rect.width(),
            (p.y - rect.top()) / rect.height(),
        )
    };
    let uvs: Vec<egui::Pos2> = pts.iter().map(|p| uv_of(*p)).collect();
    let centre_uv = uv_of(rect.center());
    let mut centre = rect.center();

    if radians.abs() > f32::EPSILON {
        paint::rotate_pts(&mut pts, rect.center(), radians);
        let mut c = [centre];
        paint::rotate_pts(&mut c, rect.center(), radians);
        centre = c[0];
    }

    let mut mesh = Mesh::with_texture(texture);
    mesh.vertices.push(Vertex {
        pos: centre,
        uv: centre_uv,
        color: tint,
    });
    for (p, uv) in pts.iter().zip(uvs.iter()) {
        mesh.vertices.push(Vertex {
            pos: *p,
            uv: *uv,
            color: tint,
        });
    }
    let n = u32::try_from(pts.len()).unwrap_or(u32::MAX);
    for i in 0..n {
        let a = i + 1;
        let b = (i + 1) % n + 1;
        mesh.add_triangle(0, a, b);
    }
    painter.add(Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_provenance_adds_nothing() {
        let c = CardChrome::for_provenance(Provenance::Window);
        assert!(!c.composites());
        assert!(!c.plate);
        assert!(!c.shadow);
        assert_eq!(c.padding, 0.0);
        assert_eq!(c.thumb_radius, 0.0);
        assert!(c.overlays_match());
    }

    #[test]
    fn other_provenances_get_a_card() {
        for p in [
            Provenance::Display,
            Provenance::Region,
            Provenance::AllDisplays,
            Provenance::Stitched,
        ] {
            let c = CardChrome::for_provenance(p);
            assert!(c.composites(), "{p:?} should get chrome");
            assert!(c.is_concentric(), "{p:?} radius must be concentric");
            assert!(c.overlays_match());
        }
    }

    #[test]
    fn fit_preserves_aspect() {
        let bounds = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0));
        let r = fit(bounds, (100, 100));
        assert!((r.width() - r.height()).abs() < 0.001);
        assert!(r.height() <= bounds.height() + 0.001);
        assert!((r.center() - bounds.center()).length() < 0.001);
    }

    #[test]
    fn fit_survives_degenerate_sizes() {
        let bounds = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0));
        assert_eq!(fit(bounds, (0, 10)), bounds);
        assert_eq!(fit(bounds, (10, 0)), bounds);
    }

    #[test]
    fn control_ids_are_stable_across_position() {
        let a = control_id(CardId(7), CardAction::Copy);
        let b = control_id(CardId(7), CardAction::Copy);
        assert_eq!(a, b);
        assert_ne!(a, control_id(CardId(8), CardAction::Copy));
        assert_ne!(a, control_id(CardId(7), CardAction::Save));
    }

    #[test]
    fn the_card_body_accessible_name_identifies_content_and_drag_action() {
        let content = CardContent::new("Shot.png", (1920, 1080), Provenance::Display);
        let label = accessible_name(&content);
        assert!(label.contains("Shot.png"), "{label}");
        assert!(label.contains("1920 × 1080"), "{label}");
        assert!(label.contains("Activate to annotate"), "{label}");
        assert!(label.contains("Drag right or up"), "{label}");
    }
}
