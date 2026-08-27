//! Turning "which parts of the overlay are real" into an input region.
//!
//! Scrozz's capture stack is a mostly-empty window: a few rounded thumbnails in
//! the bottom-left corner of a surface that is otherwise transparent. The window
//! is nonetheless a rectangle, and by default a rectangle swallows every click
//! inside it. Without input shaping, an overlay sized to hold four cards puts an
//! invisible dead zone over a large piece of the user's desktop, and the failure
//! is silent: clicks simply stop working somewhere the user cannot see anything.
//!
//! Decision D27 makes invisibility-at-rest a requirement rather than a nicety,
//! and this module is the arithmetic half of delivering it.
//!
//! # One computation, two protocols
//!
//! The same rectangle list drives both backends, because both spell the same
//! idea:
//!
//! - **X11** — `SHAPE`'s `ShapeInput` kind, set to the card rectangles. An empty
//!   list means no part of the window accepts input, which is exactly
//!   click-through.
//! - **Wayland** — `wl_surface.set_input_region`. A region with no rectangles is
//!   click-through; passing no region at all (`None`) means "infinite", i.e. the
//!   whole surface. The two are easy to confuse and mean opposite things, which
//!   is why [`InputRegion`] names them separately rather than using
//!   `Option<Vec<_>>`.
//!
//! # Rounding outward is not a detail
//!
//! Card frames are logical floats; input regions are integers. Rounding to
//! nearest would shave up to half a pixel off each edge, and the visible result
//! is a card whose border is drawn but not clickable — a hairline of dead pixels
//! exactly where a user aims when they grab the edge of a thumbnail. Every
//! rectangle here is therefore rounded *outward*: origin down, far edge up. The
//! input region is allowed to be a fraction larger than the pixels it covers,
//! and is never allowed to be smaller.
//!
//! Pure arithmetic over `scrozz_core` geometry; no X connection, no Wayland
//! socket, no `cfg(target_os)`.

use scrozz_core::LogicalRect;

/// A rectangle in surface-local integer coordinates.
///
/// Stored as `i32`, which is Wayland's own width. The X11 `SHAPE` extension uses
/// `i16`/`u16`, so [`super::x11`] clamps on the way out; doing it here would
/// throw away range the Wayland path can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionRect {
    /// Left edge, relative to the surface's own origin.
    pub x: i32,
    /// Top edge, relative to the surface's own origin.
    pub y: i32,
    /// Width in pixels. Always positive; zero-area rectangles are dropped.
    pub width: u32,
    /// Height in pixels. Always positive.
    pub height: u32,
}

/// Which parts of an overlay surface accept pointer input.
///
/// The three cases are deliberately distinct rather than an `Option<Vec<_>>`:
/// "no rectangles" and "no region" mean opposite things in Wayland, and a type
/// that cannot express the difference is a bug waiting for a compositor to find
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputRegion {
    /// The whole surface accepts input. Wayland: `set_input_region(None)`. X11:
    /// remove the input shape entirely.
    Everything,
    /// Nothing accepts input; every click reaches the desktop underneath.
    /// Wayland: an empty region. X11: an empty rectangle list.
    Nothing,
    /// Only these rectangles accept input.
    Rects(Vec<RegionRect>),
}

impl InputRegion {
    /// Whether a click anywhere on the surface would fall through.
    #[must_use]
    pub fn is_fully_click_through(&self) -> bool {
        matches!(self, Self::Nothing)
    }

    /// The rectangles, or an empty slice for the two degenerate cases.
    ///
    /// Callers that need to distinguish [`Self::Everything`] from
    /// [`Self::Nothing`] must match on the enum; this is for the common case of
    /// "hand the list to the protocol".
    #[must_use]
    pub fn rects(&self) -> &[RegionRect] {
        match self {
            Self::Rects(rects) => rects,
            Self::Everything | Self::Nothing => &[],
        }
    }
}

/// Computes the input region for an overlay window.
///
/// `window` is the surface's frame in the same coordinate space as `hits`;
/// `hits` are the rectangles that should accept clicks — card frames, the
/// capture dock — and `click_through` is whether shaping is wanted at all.
///
/// Three rules, in order:
///
/// 1. `click_through == false` means the surface is a real window that owns its
///    whole area: [`InputRegion::Everything`]. The selection overlay is this.
/// 2. No hits means nothing to click: [`InputRegion::Nothing`]. This is the
///    resting state of the capture stack and the case D27 is about.
/// 3. Otherwise, each hit is clipped to the window, translated into
///    surface-local coordinates and rounded outward. Hits that fall entirely
///    outside the window contribute nothing, and if that leaves no rectangles
///    the answer collapses back to [`InputRegion::Nothing`] rather than an empty
///    `Rects` — same meaning, but only one of the two spellings survives a
///    round-trip through a protocol.
#[must_use]
pub fn input_region(window: LogicalRect, hits: &[LogicalRect], click_through: bool) -> InputRegion {
    if !click_through {
        return InputRegion::Everything;
    }
    if hits.is_empty() {
        return InputRegion::Nothing;
    }

    let rects: Vec<RegionRect> = hits
        .iter()
        .filter_map(|hit| clip_to_local(window, *hit))
        .collect();

    if rects.is_empty() {
        InputRegion::Nothing
    } else {
        InputRegion::Rects(rects)
    }
}

/// Clips one hit rectangle to the window and moves it into surface-local space.
///
/// Returns `None` when the intersection is empty, which happens legitimately:
/// a card animating out can be momentarily outside its own window, and that
/// should quietly contribute nothing rather than produce a negative-width
/// rectangle for the compositor to reject.
fn clip_to_local(window: LogicalRect, hit: LogicalRect) -> Option<RegionRect> {
    // Finiteness is checked on the *inputs*, before any clamping, because
    // `f64::max` and `f64::min` deliberately ignore NaN and return the other
    // operand. Checking afterwards therefore checks a value from which the NaN
    // has already been laundered: a card at `x: NaN` would clamp to the window's
    // own left edge and produce a rectangle covering the whole window — which is
    // not a slightly wrong region, it is the exact opposite of the one D27 asks
    // for, and it would make every click on the desktop land on Scrozz.
    if !(window.origin.x.is_finite()
        && window.origin.y.is_finite()
        && window.size.width.is_finite()
        && window.size.height.is_finite()
        && hit.origin.x.is_finite()
        && hit.origin.y.is_finite()
        && hit.size.width.is_finite()
        && hit.size.height.is_finite())
    {
        return None;
    }

    let win_left = window.origin.x;
    let win_top = window.origin.y;
    let win_right = win_left + window.size.width;
    let win_bottom = win_top + window.size.height;

    let left = hit.origin.x.max(win_left);
    let top = hit.origin.y.max(win_top);
    let right = (hit.origin.x + hit.size.width).min(win_right);
    let bottom = (hit.origin.y + hit.size.height).min(win_bottom);

    if right <= left || bottom <= top {
        return None;
    }

    // Outward rounding, then translation. Doing it in this order keeps the
    // rounded rectangle aligned to the same pixel grid as the window itself,
    // which matters when the window origin is fractional on a scaled output.
    let x0 = (left - win_left).floor();
    let y0 = (top - win_top).floor();
    let x1 = (right - win_left).ceil();
    let y1 = (bottom - win_top).ceil();

    let width = x1 - x0;
    let height = y1 - y0;
    if width <= 0.0 || height <= 0.0 {
        return None;
    }

    Some(RegionRect {
        x: clamp_i32(x0),
        y: clamp_i32(y0),
        width: clamp_u32(width),
        height: clamp_u32(height),
    })
}

/// Saturating float-to-`i32`, because `as` on an out-of-range float is a
/// saturating cast in Rust but a silent one, and being explicit here documents
/// that the clamp is intended rather than incidental.
fn clamp_i32(value: f64) -> i32 {
    value.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

/// Saturating float-to-`u32`, clamped at zero.
fn clamp_u32(value: f64) -> u32 {
    value.clamp(0.0, f64::from(u32::MAX)) as u32
}
