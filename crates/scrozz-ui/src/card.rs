//! Painting one capture card.
//!
//! [`stack`](crate::stack) decides where every card is and how far through each
//! animation it has travelled. This module turns one of its [`CardFrame`]s into
//! pixels: the capture itself and the controls that appear on hover.
//!
//! # What a card is made of
//!
//! At rest a card is only the capture (D12). On hover a screenshot adds the
//! equal-priority **Copy** and **Save** pills plus corner actions; a recording
//! keeps only video-appropriate corner actions. The card body *is* the drag
//! handle; there is no separate grab affordance.
//!
//! # Motion applies to the card, not to its buttons
//!
//! The reveal animates because the chrome is an object arriving (D19). The
//! buttons inside it do not animate at all: hover and press are instant state
//! changes, which is why [`paint`](crate::paint)'s controls contain no easing
//! and why this module never wraps one in a fade. Instant feedback reads as
//! faster than an eased one, and a control is not an object.
//!
//! # D9: preview treatment never changes exported pixels
//!
//! The floating stack is presentation, not export. Every thumbnail is cover-fit
//! into the same fixed frame and clipped to the same radius, regardless of
//! capture type. That intentionally crops only the transient preview; the
//! capture handed to the clipboard, file encoder and editor is untouched.

use std::time::Duration;

use egui::epaint::{Mesh, Vertex};
use egui::{
    Color32, FontId, Id, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2,
};
use scrozz_core::Provenance;

use crate::icons::Icon;
use crate::motion::fade;
use crate::paint::{self, Reveal, Surface};
use crate::recording_controls::format_duration;
use crate::stack::{CardFrame, CardId, MAX_LEAN};
use crate::theme::{Radius, Space, corner};

/// Height of a revealed pill button.
const PILL_H: f32 = 30.0;
/// Side of a revealed corner icon button.
const ICON_BTN: f32 = 28.0;
/// Inset of revealed chrome from the capture's edge.
const CHROME_INSET: f32 = Space::SM;
/// Peak alpha of the hover scrim.
const HOVER_SCRIM: f32 = 132.0;
/// Transparent viewport room needed for a fully lifted card shadow to fade.
///
/// The deepest card shadow offsets 16 points and blurs 44 points. Forty-eight
/// points clears that complete falloff with a little rounding tolerance.
pub const SHADOW_BLEED: f32 = 48.0;
/// Width below which primary actions collapse to icon-only controls.
const COMPACT_CHROME_W: f32 = 154.0;
/// Height below which the two-row chrome collapses to icon-only controls.
const COMPACT_CHROME_H: f32 = 72.0;
/// Diameter of the resting play badge on a full-size card, in points.
const PLAY_BADGE: f32 = 46.0;
/// Smallest legible play badge; below this the badge is dropped entirely.
const PLAY_BADGE_MIN: f32 = 22.0;
/// Alpha of the play badge's disc.
const PLAY_BADGE_SCRIM: f32 = 150.0;
/// Alpha of the duration chip's plate.
const DURATION_CHIP_SCRIM: f32 = 168.0;
/// Point size of the duration chip's label.
const DURATION_TEXT: f32 = 11.0;
// ---------------------------------------------------------------------------
// Chrome
// ---------------------------------------------------------------------------

/// The geometry a card is allowed to add around a capture.
///
/// Accepts [`Provenance`] at the boundary so future platform-specific treatment
/// cannot bypass this decision. Today every provenance intentionally resolves
/// to the same preview chrome.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CardChrome {
    /// Corner radius applied to the preview, in points.
    pub thumb_radius: f32,
    /// Radius for anything drawn over the shared preview container.
    pub overlay_radius: f32,
    /// Inset of the capture inside the card rectangle, in points.
    pub padding: f32,
    /// Whether a synthetic backing plate is drawn behind the capture.
    pub plate: bool,
    /// Whether a synthetic drop shadow is drawn under the card.
    pub shadow: bool,
    /// Whether the preview draws a hairline around the thumbnail.
    ///
    /// The border is preview chrome and is never written into exported pixels.
    pub capture_border: bool,
}

impl CardChrome {
    /// The card's own outer radius.
    pub const OUTER_RADIUS: f32 = Radius::CARD;

    /// Padding between the card edge and the capture.
    ///
    /// Zero by design: the image is the thumbnail surface and fills it edge to
    /// edge; cover-fit UVs handle the intentional preview-only crop.
    pub const PADDING: f32 = 0.0;

    /// The chrome permitted for a capture of this provenance.
    #[must_use]
    pub fn for_provenance(provenance: Provenance) -> Self {
        let _ = provenance;
        Self {
            thumb_radius: Self::OUTER_RADIUS,
            overlay_radius: Self::OUTER_RADIUS,
            padding: Self::PADDING,
            plate: true,
            shadow: true,
            capture_border: true,
        }
    }

    /// Whether this chrome supplies the shared floating preview container.
    #[must_use]
    pub fn has_preview_container(self) -> bool {
        self.plate && self.shadow
    }

    /// Whether overlays use the preview container's own rounding.
    #[must_use]
    pub fn overlays_match_container(self) -> bool {
        (self.overlay_radius - Self::OUTER_RADIUS).abs() < f32::EPSILON
    }

    /// Whether `inner = outer − padding` holds.
    #[must_use]
    pub fn is_concentric(self) -> bool {
        (Self::OUTER_RADIUS - self.padding - self.thumb_radius).abs() < 0.001
    }

    /// Uses the complete fixed slot for both the container and its preview.
    #[must_use]
    pub fn geometry(self, slot: Rect, source_px: (u32, u32)) -> CardGeometry {
        let _ = (self, source_px);
        CardGeometry {
            container: slot,
            capture: slot,
        }
    }
}

/// The visible geometry derived from one fixed stack slot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CardGeometry {
    /// Shared outer plate, shadow, hover surface and interaction hitbox.
    pub container: Rect,
    /// Complete thumbnail rectangle; aspect preservation happens in its UV crop.
    pub capture: Rect,
}

// ---------------------------------------------------------------------------
// Content and results
// ---------------------------------------------------------------------------

/// What kind of finished media a card is presenting.
///
/// The card geometry, cover-fill and drag handle are identical for both: a
/// recording is a capture like any other, and D9's "preview treatment never
/// changes exported pixels" rule applies unchanged. Only two things differ — a
/// video says it is a video (play badge and duration), and its bottom-left
/// hover action opens the video editor rather than the annotation editor.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum CardMedia {
    /// A still image capture.
    #[default]
    Image,
    /// A finalized recording whose durable media file outlives the card.
    Video {
        /// Native container duration, shown as the card's duration chip.
        duration: Duration,
        /// Whether the durable source carries audio.
        has_audio: bool,
    },
}

impl CardMedia {
    /// A video card for a recording of `duration`.
    #[must_use]
    pub const fn video(duration: Duration, has_audio: bool) -> Self {
        Self::Video {
            duration,
            has_audio,
        }
    }

    /// Whether this card presents playable video.
    #[must_use]
    pub const fn is_video(self) -> bool {
        matches!(self, Self::Video { .. })
    }

    /// The duration to display, when there is one.
    #[must_use]
    pub const fn duration(self) -> Option<Duration> {
        match self {
            Self::Image => None,
            Self::Video { duration, .. } => Some(duration),
        }
    }

    /// The hover action occupying the bottom-left corner for this media.
    ///
    /// One place decides it, so a video can never sprout an annotation editor
    /// it has no document for, and a screenshot can never offer a trim UI.
    #[must_use]
    pub const fn edit_action(self) -> CardAction {
        match self {
            Self::Image => CardAction::Annotate,
            Self::Video { .. } => CardAction::Edit,
        }
    }
}

/// Everything the painter needs to know about one capture.
///
/// A borrowed view, not an owner: the overlay keeps the textures and strings, and
/// a still render can synthesise one without allocating a capture.
#[derive(Clone, Copy, Debug)]
pub struct CardContent<'a> {
    /// File name retained for accessibility and future detail surfaces.
    pub name: &'a str,
    /// Capture size in pixels, retained for accessibility and detail surfaces.
    pub source_px: (u32, u32),
    /// Where the pixels came from. Decides the chrome (D9).
    pub provenance: Provenance,
    /// Whether this card is a still or a recording.
    pub media: CardMedia,
    /// The uploaded thumbnail, if it has been uploaded yet.
    pub texture: Option<egui::TextureId>,
}

impl<'a> CardContent<'a> {
    /// A capture with no thumbnail uploaded yet.
    #[must_use]
    pub fn new(name: &'a str, source_px: (u32, u32), provenance: Provenance) -> Self {
        Self {
            name,
            source_px,
            provenance,
            media: CardMedia::Image,
            texture: None,
        }
    }

    /// The same content with a thumbnail attached.
    #[must_use]
    pub fn with_texture(mut self, texture: egui::TextureId) -> Self {
        self.texture = Some(texture);
        self
    }

    /// The same content presented as the given media kind.
    #[must_use]
    pub const fn with_media(mut self, media: CardMedia) -> Self {
        self.media = media;
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
    /// Copy the capture to the clipboard.
    Copy,
    /// Save the capture to disk.
    Save,
    /// Open the annotation editor.
    Annotate,
    /// Open the video editor for a recording.
    Edit,
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
            Self::Edit => "edit",
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
            Self::Edit => "Edit",
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
            Self::Edit => Icon::Video,
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
    let geometry = chrome.geometry(frame.rect, content.source_px);
    let rect = geometry.container;
    let alpha = frame.alpha.clamp(0.0, 1.0);
    let palette = surface.palette();

    // The body is sensed before anything is painted so the response is available
    // to the chrome, and so the card sits *under* its own buttons in the layer
    // order rather than swallowing their clicks.
    let body = ui.interact(rect, body_id(frame.id), Sense::click_and_drag());

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

    let capture = geometry.capture;
    draw_capture(ui, surface, content, chrome, capture, angle, alpha);
    draw_media_marks(ui, content, capture, alpha);

    let reveal = frame.reveal.clamp(0.0, 1.0) * flat;
    let action = draw_chrome(
        ui,
        surface,
        frame,
        chrome,
        content.media,
        rect,
        alpha * reveal,
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
        // No pixels yet, so use the same silhouette the cover-filled thumbnail
        // will occupy when its texture arrives.
        let fill = fade(palette.card_fill, alpha * 0.9);
        if angle.abs() > f32::EPSILON {
            paint::fill_rot_round_rect(
                painter,
                capture,
                CardChrome::OUTER_RADIUS,
                capture.center(),
                angle,
                fill,
            );
        } else {
            painter.rect_filled(capture, corner(CardChrome::OUTER_RADIUS), fill);
        }
        return;
    };

    // Every capture type fills the same preview frame. Cropping exists only in
    // this texture mapping; the capture itself is never mutated.
    let uv = cover_uv(capture, content.source_px);
    let tint = Color32::WHITE.gamma_multiply(alpha);
    textured_round_rect_uv(
        painter,
        texture,
        capture,
        uv,
        chrome.thumb_radius,
        angle,
        tint,
    );

    if chrome.capture_border {
        painter.rect_stroke(
            capture,
            corner(chrome.thumb_radius),
            Stroke::new(1.0, fade(palette.thumb_border, alpha)),
            StrokeKind::Inside,
        );
    }
}

/// Says "this is a recording" without claiming anything about its contents.
///
/// Drawn after the poster and before the hover chrome, so the same scrim that
/// dims the thumbnail dims these too rather than leaving a badge floating over
/// a darkened card. A still capture draws nothing here at all.
fn draw_media_marks(ui: &Ui, content: &CardContent<'_>, capture: Rect, alpha: f32) {
    let CardMedia::Video { duration, .. } = content.media else {
        return;
    };
    if alpha <= 0.004 {
        return;
    }
    draw_play_badge(ui, capture, alpha);
    draw_duration_chip(ui, capture, duration, alpha);
}

fn draw_play_badge(ui: &Ui, capture: Rect, alpha: f32) {
    let diameter = PLAY_BADGE
        .min(capture.width() * 0.34)
        .min(capture.height() * 0.42);
    if diameter < PLAY_BADGE_MIN {
        return;
    }
    let painter = ui.painter();
    let centre = capture.center();
    let radius = diameter * 0.5;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scrim = (PLAY_BADGE_SCRIM * alpha).round().clamp(0.0, 255.0) as u8;
    painter.circle_filled(centre, radius, Color32::from_black_alpha(scrim));
    painter.circle_stroke(
        centre,
        radius,
        Stroke::new(1.0, Color32::WHITE.gamma_multiply(alpha * 0.28)),
    );

    // A hand-built triangle rather than an icon: the glyph has to sit on the
    // disc's optical centre, which is right of its geometric one, and no icon
    // asset can express that offset.
    let edge = diameter * 0.34;
    let half = edge * 0.5;
    let nudge = edge * 0.16;
    let tip = pos2(centre.x + half + nudge, centre.y);
    let top = pos2(centre.x - half + nudge, centre.y - edge * 0.58);
    let bottom = pos2(centre.x - half + nudge, centre.y + edge * 0.58);
    painter.add(Shape::convex_polygon(
        vec![top, tip, bottom],
        Color32::WHITE.gamma_multiply(alpha),
        Stroke::NONE,
    ));
}

fn draw_duration_chip(ui: &Ui, capture: Rect, duration: Duration, alpha: f32) {
    let label = format_duration(duration);
    let painter = ui.painter();
    let font = FontId::proportional(DURATION_TEXT);
    let galley = painter.layout_no_wrap(
        label,
        font,
        Color32::WHITE.gamma_multiply(alpha.clamp(0.0, 1.0)),
    );
    let padding = vec2(Space::XS, 2.0);
    let plate = Rect::from_min_size(Pos2::ZERO, galley.size() + padding * 2.0);
    if plate.width() > capture.width() - Space::SM || plate.height() > capture.height() - Space::SM
    {
        return;
    }
    let origin = pos2(
        capture.right() - Space::XS - plate.width(),
        capture.bottom() - Space::XS - plate.height(),
    );
    let plate = Rect::from_min_size(origin, plate.size());
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scrim = (DURATION_CHIP_SCRIM * alpha).round().clamp(0.0, 255.0) as u8;
    painter.rect_filled(
        plate,
        corner(Radius::pill(plate.height())),
        Color32::from_black_alpha(scrim),
    );
    painter.galley(plate.min + padding, galley, Color32::WHITE);
}

/// The hover chrome: a scrim, two equal neutral pills, four matching corner buttons.
///
/// Returns the action pressed this frame. The controls themselves are drawn by
/// [`paint`], which contains no animation — the fade lives entirely in the
/// [`Reveal`] passed through them (D19).
fn draw_chrome(
    ui: &mut Ui,
    surface: &Surface<'_>,
    frame: &CardFrame,
    chrome: CardChrome,
    media: CardMedia,
    container: Rect,
    opacity: f32,
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
            container,
            corner(chrome.overlay_radius),
            Color32::from_black_alpha(scrim),
        );
    }

    let inner = container.shrink(CHROME_INSET);

    let lift = Reveal::new(opacity, vec2(0.0, (1.0 - opacity) * 6.0));
    let settle = Reveal::new(opacity, vec2(0.0, (1.0 - opacity) * -3.0));
    let mut pressed = None;

    if inner.width() < COMPACT_CHROME_W || inner.height() < COMPACT_CHROME_H {
        let rects = compact_primary_rects(inner)?;
        let actions = if media.is_video() {
            [CardAction::Edit, CardAction::Close]
        } else {
            [CardAction::Copy, CardAction::Save]
        };
        for (rect, action) in rects.into_iter().zip(actions) {
            let response = paint::card_icon_button(
                ui,
                surface,
                rect,
                control_id(frame.id, action),
                action.icon(),
                action.label(),
                lift,
            );
            if response.clicked() {
                pressed = Some(action);
            }
        }
        return pressed;
    }

    if !media.is_video() {
        // Copy and Save share one equal-priority treatment for still images.
        for (r, action) in primary_pill_rects(inner)?
            .into_iter()
            .zip([CardAction::Copy, CardAction::Save])
        {
            let resp = paint::card_pill_button(
                ui,
                surface,
                r,
                control_id(frame.id, action),
                action.icon(),
                action.label(),
                lift,
            );
            if resp.clicked() {
                pressed = Some(action);
            }
        }
    }

    // Smaller matching controls occupy the corners. Close follows native window
    // convention: left on macOS, right on Windows and Linux.
    let size = vec2(ICON_BTN, ICON_BTN);
    let corners = corner_actions(media);
    let origins = [
        inner.left_top(),
        pos2(inner.right() - ICON_BTN, inner.top()),
        pos2(inner.left(), inner.bottom() - ICON_BTN),
        pos2(inner.right() - ICON_BTN, inner.bottom() - ICON_BTN),
    ];
    for (action, origin) in corners.into_iter().zip(origins) {
        let Some(action) = action else {
            continue;
        };
        let r = Rect::from_min_size(origin, size);
        let resp = paint::card_icon_button(
            ui,
            surface,
            r,
            control_id(frame.id, action),
            action.icon(),
            action.label(),
            settle,
        );
        if resp.clicked() {
            pressed = Some(action);
        }
    }

    pressed
}

/// The four corner slots, in origin order: top-left, top-right, bottom-left,
/// bottom-right.
///
/// Two slots vary with the media, and both for the same reason — a control that
/// is drawn and then always fails is worse than one that was never offered:
///
/// * **Bottom-left** is the edit affordance. A still gets **Annotate**, a
///   recording gets **Edit**; [`CardMedia::edit_action`] is the one place that
///   decision is made.
/// * **Pin** is empty for a recording. Pin to Screen holds a still image in a
///   floating window and has nothing to show for a video, which is exactly what
///   the After Capture matrix already says about `record.pin-to-screen`.
fn corner_actions_for(close_on_left: bool, media: CardMedia) -> [Option<CardAction>; 4] {
    let pin = (!media.is_video()).then_some(CardAction::Pin);
    let close = Some(CardAction::Close);
    let top = if close_on_left {
        [close, pin]
    } else {
        [pin, close]
    };
    [
        top[0],
        top[1],
        Some(media.edit_action()),
        Some(CardAction::Upload),
    ]
}

fn corner_actions(media: CardMedia) -> [Option<CardAction>; 4] {
    corner_actions_for(cfg!(target_os = "macos"), media)
}

fn primary_pill_rects(inner: Rect) -> Option<[Rect; 2]> {
    let gap = Space::SM;
    let total_h = PILL_H.mul_add(2.0, gap);
    if inner.width() < PILL_H || inner.height() < total_h {
        return None;
    }
    let width = inner.width().min(112.0);
    let first = Rect::from_min_size(
        pos2(
            inner.center().x - width * 0.5,
            inner.center().y - total_h * 0.5,
        ),
        vec2(width, PILL_H),
    );
    Some([first, first.translate(vec2(0.0, PILL_H + gap))])
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

fn compact_primary_rects(inner: Rect) -> Option<[Rect; 2]> {
    let gap = Space::SM;
    let total = ICON_BTN.mul_add(2.0, gap);
    if inner.width() >= total && inner.height() >= ICON_BTN {
        let first = Rect::from_min_size(
            pos2(
                inner.center().x - total * 0.5,
                inner.center().y - ICON_BTN * 0.5,
            ),
            vec2(ICON_BTN, ICON_BTN),
        );
        return Some([first, first.translate(vec2(ICON_BTN + gap, 0.0))]);
    }
    if inner.height() >= total && inner.width() >= ICON_BTN {
        let first = Rect::from_min_size(
            pos2(
                inner.center().x - ICON_BTN * 0.5,
                inner.center().y - total * 0.5,
            ),
            vec2(ICON_BTN, ICON_BTN),
        );
        return Some([first, first.translate(vec2(0.0, ICON_BTN + gap))]);
    }
    None
}

/// The centred texture coordinates that cover `bounds` with `source_px`.
///
/// The returned UV rectangle is always inside `0..=1`. A wide source crops its
/// left and right edges; a tall source crops its top and bottom edges.
#[must_use]
pub fn cover_uv(bounds: Rect, source_px: (u32, u32)) -> Rect {
    let full = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
    let (w, h) = source_px;
    if w == 0 || h == 0 || bounds.width() <= 0.0 || bounds.height() <= 0.0 {
        return full;
    }
    #[allow(clippy::cast_precision_loss)]
    let source_aspect = w as f32 / h as f32;
    let target_aspect = bounds.width() / bounds.height();
    if source_aspect > target_aspect {
        let visible = target_aspect / source_aspect;
        let inset = (1.0 - visible) * 0.5;
        Rect::from_min_max(pos2(inset, 0.0), pos2(1.0 - inset, 1.0))
    } else {
        let visible = source_aspect / target_aspect;
        let inset = (1.0 - visible) * 0.5;
        Rect::from_min_max(pos2(0.0, inset), pos2(1.0, 1.0 - inset))
    }
}

/// Draw a texture into a rounded — and optionally rotated — rectangle.
///
/// egui has no rounded image primitive and no rotation primitive, so this builds
/// the mesh: a triangle fan over [`paint::rounded_poly`], with UVs taken from the
/// *unrotated* geometry so the image rotates with the shape instead of sliding
/// under it. A `radius` of zero degenerates to a plain quad for callers that do
/// not want clipping.
pub fn textured_round_rect(
    painter: &egui::Painter,
    texture: egui::TextureId,
    rect: Rect,
    radius: f32,
    radians: f32,
    tint: Color32,
) {
    textured_round_rect_uv(
        painter,
        texture,
        rect,
        Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)),
        radius,
        radians,
        tint,
    );
}

fn textured_round_rect_uv(
    painter: &egui::Painter,
    texture: egui::TextureId,
    rect: Rect,
    uv: Rect,
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
            uv.left() + (p.x - rect.left()) / rect.width() * uv.width(),
            uv.top() + (p.y - rect.top()) / rect.height() * uv.height(),
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
    fn window_provenance_uses_the_same_fixed_preview_chrome() {
        let c = CardChrome::for_provenance(Provenance::Window);
        assert!(c.has_preview_container());
        assert!(c.plate);
        assert!(c.shadow);
        assert_eq!(c.padding, CardChrome::PADDING);
        assert_eq!(c.thumb_radius, CardChrome::OUTER_RADIUS);
        assert!(c.capture_border);
        assert!(c.overlays_match_container());
        assert!(c.is_concentric());
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
            assert!(c.has_preview_container(), "{p:?} should get chrome");
            assert!(c.is_concentric(), "{p:?} radius must be concentric");
            assert!(c.capture_border);
            assert!(c.overlays_match_container());
        }
    }

    #[test]
    fn every_source_uses_the_complete_fixed_preview() {
        let slot = Rect::from_min_size(pos2(0.0, 0.0), vec2(210.0, 150.0));
        for source in [(100, 100), (1600, 1000), (1920, 1080), (900, 1600)] {
            let chrome = CardChrome::for_provenance(Provenance::Display);
            let geometry = chrome.geometry(slot, source);
            assert_eq!(geometry.container, slot, "{source:?}");
            assert_eq!(geometry.capture, slot, "{source:?}");
        }
    }

    #[test]
    fn primary_actions_form_one_vertical_spine() {
        let slot = Rect::from_min_size(pos2(0.0, 0.0), vec2(210.0, 150.0));
        let chrome = CardChrome::for_provenance(Provenance::Display);
        for source in [
            (5760, 1080),
            (900, 1600),
            (1179, 2556),
            (3000, 300),
            (300, 3000),
        ] {
            let inner = chrome.geometry(slot, source).container.shrink(CHROME_INSET);
            let controls = primary_pill_rects(inner)
                .unwrap_or_else(|| panic!("{source:?} lost Copy and Save controls"));
            assert!(inner.contains_rect(controls[0]), "{source:?}");
            assert!(inner.contains_rect(controls[1]), "{source:?}");
            assert_eq!(controls[0].center().x, controls[1].center().x);
            assert!(controls[0].bottom() < controls[1].top());
        }
    }

    #[test]
    fn close_follows_mac_and_windows_corner_conventions() {
        assert_eq!(
            corner_actions_for(true, CardMedia::Image),
            [
                Some(CardAction::Close),
                Some(CardAction::Pin),
                Some(CardAction::Annotate),
                Some(CardAction::Upload),
            ]
        );
        assert_eq!(
            corner_actions_for(false, CardMedia::Image),
            [
                Some(CardAction::Pin),
                Some(CardAction::Close),
                Some(CardAction::Annotate),
                Some(CardAction::Upload),
            ]
        );
    }

    #[test]
    fn a_recording_card_offers_edit_and_never_offers_pin() {
        let video = CardMedia::video(Duration::from_secs(9), false);
        for close_on_left in [true, false] {
            let corners = corner_actions_for(close_on_left, video);
            assert!(
                !corners.contains(&Some(CardAction::Pin)),
                "Pin to Screen holds a still image and would always fail here"
            );
            assert!(corners.contains(&Some(CardAction::Close)), "close stays");
            assert!(
                corners.contains(&Some(CardAction::Edit)),
                "the bottom-left slot opens the video editor"
            );
            assert!(!corners.contains(&Some(CardAction::Annotate)));
            assert_eq!(corners[2], Some(CardAction::Edit), "bottom-left slot");
        }
    }

    #[test]
    fn cover_uv_crops_wide_and_tall_sources_around_their_centres() {
        let bounds = Rect::from_min_size(pos2(0.0, 0.0), vec2(210.0, 150.0));
        let exact = cover_uv(bounds, (1400, 1000));
        assert_eq!(exact, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)));

        let wide = cover_uv(bounds, (2000, 1000));
        assert!((wide.left() - 0.15).abs() < 0.001);
        assert!((wide.right() - 0.85).abs() < 0.001);
        assert_eq!((wide.top(), wide.bottom()), (0.0, 1.0));

        let tall = cover_uv(bounds, (1000, 1000));
        assert_eq!((tall.left(), tall.right()), (0.0, 1.0));
        assert!((tall.top() - 1.0 / 7.0).abs() < 0.001);
        assert!((tall.bottom() - 6.0 / 7.0).abs() < 0.001);
    }

    #[test]
    fn cover_uv_survives_degenerate_sizes() {
        let bounds = Rect::from_min_size(pos2(0.0, 0.0), vec2(210.0, 150.0));
        let full = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
        assert_eq!(cover_uv(bounds, (0, 10)), full);
        assert_eq!(cover_uv(bounds, (10, 0)), full);
    }

    #[test]
    fn control_ids_are_stable_across_position() {
        let a = control_id(CardId(7), CardAction::Copy);
        let b = control_id(CardId(7), CardAction::Copy);
        assert_eq!(a, b);
        assert_ne!(a, control_id(CardId(8), CardAction::Copy));
        assert_ne!(a, control_id(CardId(7), CardAction::Save));
    }
}
