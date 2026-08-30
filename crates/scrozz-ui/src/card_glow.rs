//! The landing glow: how a freshly arrived capture card announces itself.
//!
//! A card that has just finished sliding into the stack lights up for about
//! six seconds — an even wash of light across its thumbnail, and then a
//! coloured aurora that blooms outward from its edge, turns slowly, and fades
//! to nothing. It is one event on one clock: the wash is a front travelling
//! out from the centre, and the rim's ignition is timed against the instant
//! that front reaches the border, leading it slightly so the edge is already
//! answering as the light lands.
//!
//! # What it will not do
//!
//! * **Nothing before the card has settled.** The animation starts from
//!   [`CardFrame::landed`](crate::stack::CardFrame::landed), which is `None`
//!   until the entry motion is over, and for cards the stack was seeded with
//!   at startup it is `None` forever.
//! * **Nothing under reduce-motion.** D13: this is decoration, and decoration
//!   is the first thing to go.
//! * **Nothing on a card being edited.** An editing card has exactly one
//!   thing to say about itself, and [`draw_editing_pill`](super::draw_card)
//!   is saying it.
//! * **Nothing once the window is over.** [`is_active`] is what the frame
//!   scheduler asks, so a settled pile requests no repaints at all — there is
//!   no idle 60 Hz here, and a hidden overlay asks for nothing.
//!
//! # Colour comes from the capture
//!
//! [`sample_accent`] reads three dominant hues weighted by saturation and
//! brightness, so what is *present* beats what merely covers area — a mostly
//! white document with one blue toolbar lights blue. It also reports how
//! colourful the capture was at all, and a grey screenshot lights its rim
//! white rather than having a hue invented for it.
//!
//! # Why the rim is a texture and not a mesh
//!
//! egui has no shaders and no gradients: a shape is triangles with one colour
//! per vertex, interpolated affinely. That reproduces a gradient exactly only
//! when the colour is a *linear* function of position. A soft glow is not —
//! it is a steep, curved falloff — so approximating it with geometry always
//! shows: faceted wedges from a triangle fan, visible seams from a ribbon
//! strip. Subdividing harder only moves the seams around.
//!
//! So the falloff is not geometry. [`rim_texture`] bakes the whole profile
//! into an alpha image once per card size — one texel per point, the exact
//! rounded-rect signed distance, a power taper across it — and the mesh
//! carries only the broad, slow brightness envelope, which is what vertex
//! interpolation is actually good at. Sharp detail from the sampler, smooth
//! detail from the vertices, neither doing the other's job.
//!
//! The surface wash stays a mesh: its gradient is wide and gentle enough that
//! a dense regular grid carries it cleanly.

use egui::epaint::{Mesh, Vertex};
use egui::{Color32, ColorImage, Painter, Pos2, Rect, Shape, TextureHandle, TextureOptions, pos2};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// The treatment
// ---------------------------------------------------------------------------
//
// One tuning, chosen from five. The four that lost did so on the seam between
// the two halves: a surface wash that crosses in a blink followed by a rim
// that crawls reads as two animations sharing a card, however good each one
// looks alone. This one anticipates hardest — the border is already lit
// before the light reaches it — which is what stopped it reading as two
// events at all. Every number is seconds since the card settled, or alpha.

/// Total play time, in seconds. Past this the whole treatment is a no-op, so
/// a settled pile costs nothing — no drawing, and no repaint requests.
pub const GLOW_WINDOW: f32 = 6.30;

/// When the expanding wash front reaches the border. The rim's timing is
/// defined against this, which is what makes the two read as one event.
const PULSE_HIT: f32 = 1.10;
/// Brightest alpha the surface wash reaches.
const PULSE_PEAK: f32 = 0.41;
/// An extra bloom across the whole thumbnail at the instant the front reaches
/// the border, decaying over a quarter second.
///
/// The travelling crest alone is a thin thing — at any one moment most of the
/// surface is untouched by it — so however bright it is, the arrival reads as
/// a highlight sliding past rather than as the card being hit by light. This
/// is the hit.
const PULSE_FLASH: f32 = 0.27;
/// Half-width of the travelling crest, where the border is `1.0`.
const PULSE_BAND: f32 = 0.90;
/// How much the front decelerates on its way out.
///
/// A hard ease-out leaves the crest almost stationary as it lands, which is
/// graceful alone but makes what follows look like it started from a
/// standstill.
const PULSE_EASE: f32 = 0.20;
/// How much light the crest leaves behind it, so the surface reads as lit and
/// going dark rather than as a ring passing over it.
const PULSE_WAKE: f32 = 0.60;
/// How long the surface takes to go dark after contact.
const PULSE_DECAY: f32 = 0.65;

/// How far *before* the surface light reaches the border the rim starts
/// coming up, in seconds.
///
/// Measured against `contact_time`, not against [`PULSE_HIT`]. The crest is
/// wide — most of a card across — so its light washes the border long before
/// its centre gets there, and timing the rim off the centre made it look late
/// no matter what the number said.
const RIM_LEAD: f32 = 0.85;
/// How long the rim takes to reach full brightness once it starts.
const RIM_IGNITE: f32 = 0.38;
/// The settled rim's alpha.
const RIM_PEAK: f32 = 0.46;
/// Brightness multiplier at ignition, easing back to `1.0`.
const RIM_OVERSHOOT: f32 = 1.0;
/// How long the settled rim holds before it begins to fade.
const RIM_HOLD: f32 = 3.20;
/// How long that fade takes. It reaches true zero inside [`GLOW_WINDOW`].
const RIM_FADE: f32 = 2.00;
/// How far the rim reaches outward past the card's own edge, in points.
const HALO_OUT: f32 = 54.0;
/// Alpha of the hairline drawn on the border itself.
const RIM_EDGE: f32 = 0.82;
/// How long the halo takes to bloom out to that width, in seconds.
///
/// It starts tight against the border, so the glow arrives *from* the edge
/// rather than switching on at full thickness and merely getting brighter.
const RIM_GROW: f32 = 0.90;
/// How many bright lobes travel around the rim.
const RIM_LOBES: f32 = 3.0;
/// How far the troughs between those lobes dim, `0.0..=1.0`.
const RIM_LOBE_DEPTH: f32 = 0.45;
/// How fast the lobes travel around the border at ignition, in turns per
/// second.
const RIM_SPIN: f32 = 0.22;
/// The speed the lobes settle to.
const RIM_SPIN_END: f32 = 0.22;
/// How long shedding that momentum takes, as a time constant in seconds.
///
/// The lobes never stop: they are still turning as the whole thing goes out,
/// so what the eye loses at the end is the light, not the motion.
const RIM_SPIN_DECAY: f32 = 1.0;
/// How much of the capture's own colour reaches the halo's lobes. Scaled
/// again by how colourful the capture actually is, so a grey screenshot stays
/// monochrome on its own.
const RIM_TINT: f32 = 0.60;
/// The same, for the hairline on the border.
const EDGE_TINT: f32 = 0.50;
/// The same, for the surface wash. Kept low: this one lies over the picture
/// itself, where more than a tone competes with the content.
const PULSE_TINT: f32 = 0.18;

/// How far the rim reaches inward over the thumbnail, in points.
///
/// This is a rim light on the picture; the interesting part of the effect
/// happens outside the card.
const HALO_IN: f32 = 11.0;

/// How much of the treatment survives on a dark appearance.
///
/// Added light reads far harder against a dark surface than a light one: the
/// same alpha is a small step up from white and a large one up from near
/// black. Scaling it down in dark mode is what keeps the effect feeling like
/// one decision in both, rather than tasteful in one and shouting in the
/// other.
const DARK_SCALE: f32 = 0.78;

/// Whether the landing glow has anything to draw, and therefore whether the
/// frame scheduler needs to keep repainting.
///
/// The single predicate both the painter and
/// [`Activity`](crate::motion::Activity) consult, so a card can never be
/// drawing light nobody is scheduling frames for, nor scheduling frames for
/// light nobody is drawing.
#[must_use]
pub fn is_active(landed: Option<f32>, editing: bool, reduce_motion: bool) -> bool {
    if editing || reduce_motion {
        return false;
    }
    landed.is_some_and(|landed| (0.0..GLOW_WINDOW).contains(&landed))
}

/// Paints the landing treatment for a card that settled `landed` seconds ago.
pub fn draw_landing_glow(
    painter: &Painter,
    capture: Rect,
    radius: f32,
    landed: f32,
    accent: Option<&Accent>,
    dark: bool,
) {
    if !is_active(Some(landed), false, false) || capture.width() <= 0.0 || capture.height() <= 0.0 {
        return;
    }
    let tone = if dark { DARK_SCALE } else { 1.0 };
    pulse(painter, capture, radius, landed, accent, tone);
    rim(painter, capture, radius, landed, accent, tone);
}

// ---------------------------------------------------------------------------
// Small shaping helpers
// ---------------------------------------------------------------------------

/// Ease-out cubic, for a move that should set off promptly and arrive gently.
fn ease_out(t: f32) -> f32 {
    let inv = 1.0 - t.clamp(0.0, 1.0);
    1.0 - inv * inv * inv
}

/// Hermite `smoothstep` on an already-normalised `0.0..=1.0` input: flat at
/// both ends, so anything shaped by it has no edge to catch the eye.
fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// `0.0` before `a`, `1.0` after `b`, smooth in between.
fn ramp(x: f32, a: f32, b: f32) -> f32 {
    if (b - a).abs() < f32::EPSILON {
        return f32::from(x >= b);
    }
    smooth((x - a) / (b - a))
}

fn white(alpha: f32) -> Color32 {
    Color32::from_white_alpha((alpha.clamp(0.0, 1.0) * 255.0) as u8)
}

/// Where the lobes are pointing at time `s`, in radians.
///
/// Measured from the moment the card lands, not from the rim's ignition, so
/// the pattern is already turning while the pulse is still crossing the
/// surface. Both passes read this same clock: there is one light field here,
/// and the pulse and the rim are two ways of looking at it.
///
/// The angle is the exact integral of an angular velocity easing from
/// `RIM_SPIN` to `RIM_SPIN_END`, so the lobes shed the
/// speed they started with instead of changing gear.
fn lobe_phase(s: f32) -> f32 {
    let tau = RIM_SPIN_DECAY.max(0.01);
    let turns = RIM_SPIN_END.mul_add(
        s,
        (RIM_SPIN - RIM_SPIN_END) * tau * (1.0 - (-s / tau).exp()),
    );
    std::f32::consts::TAU * turns
}

/// The field's brightness at an angle: `1.0` on a lobe, falling to
/// `1.0 - depth` between them.
fn lobe_at(theta: f32, phase: f32, lobes: f32, depth: f32) -> f32 {
    if lobes <= 0.0 {
        return 1.0;
    }
    // Two harmonics at different rates, drifting against each other. A
    // single cosine gives evenly spaced lobes of matched size, which reads
    // as a mechanism; real light on an edge is lumpier than that, and the
    // beat between these two never quite repeats over the life of the glow.
    let a = (lobes * theta - phase).cos();
    let b = ((lobes + 2.0) * theta)
        .mul_add(1.0, phase.mul_add(0.6, 1.3))
        .cos();
    let lobe = 0.68f32.mul_add(a, 0.32 * b).mul_add(0.5, 0.5);
    1.0 - depth + depth * lobe
}

/// The angle of a point in the card's own aspect, so the field is not
/// stretched by a wide card: a lobe crosses a long side at the same rate it
/// crosses a short one.
fn aspect_angle(x: f32, y: f32) -> f32 {
    y.atan2(x)
}

/// Distance from the centre in the card's own aspect, `1.0` at the border.
///
/// A high-order p-norm rather than a circle, so the pulse front is the shape
/// of the card and arrives along the whole edge together.
fn norm_dist(dx: f32, dy: f32) -> f32 {
    (dx.abs().powi(6) + dy.abs().powi(6)).powf(1.0 / 6.0)
}

// ---------------------------------------------------------------------------
// Surface: a front of light expands from the centre and off the edge.
// ---------------------------------------------------------------------------

/// How many rows/columns to sample the surface at. Cheap — a few thousand
/// vertices — and dense enough for a gradient this wide and gentle.
const GRID_ROWS: usize = 20;
const GRID_COLS: usize = 48;

/// Where the crest's centre is at normalised time `t`, `1.0` at the border.
///
/// Between a constant-speed front and one that decelerates into the edge.
/// Constant hands off to the rim without a visible gear change.
fn front_at(t: f32) -> f32 {
    t + (ease_out(t) - t) * PULSE_EASE
}

#[allow(clippy::too_many_arguments)]
fn pulse(painter: &Painter, rect: Rect, radius: f32, s: f32, accent: Option<&Accent>, tone: f32) {
    // Reaches the border exactly at `pulse_hit`, then keeps going so the
    // crest's trailing half follows it off the edge rather than stopping.
    let front = if s <= PULSE_HIT {
        front_at(s / PULSE_HIT)
    } else {
        (s - PULSE_HIT) / PULSE_DECAY * PULSE_BAND + 1.0
    };
    let overall = 1.0 - smooth((s - PULSE_HIT) / PULSE_DECAY);
    // Peaks exactly on contact, gone a quarter second later.
    let flash = PULSE_FLASH
        * if s <= PULSE_HIT {
            smooth(s / (PULSE_HIT * 0.55).max(0.01))
        } else {
            1.0 - smooth((s - PULSE_HIT) / 0.25)
        };
    if overall <= 0.004 && flash <= 0.004 {
        return;
    }
    // A pulse of fixed alpha is invisible on a white capture and glaring on a
    // dark one; this is the headroom the capture actually leaves.
    let scale = accent.map_or(1.0, Accent::surface_scale);
    scanline_mesh(painter, rect, radius, |u, v| {
        let (dx, dy) = ((u - 0.5) * 2.0, (v - 0.5) * 2.0);
        let d = norm_dist(dx, dy);
        let x = ((d - front) / PULSE_BAND).clamp(-1.0, 1.0);
        // Raised cosine: zero *and* flat where it meets the untouched
        // thumbnail, so the crest has no locatable edge.
        let crest = 0.5 * (1.0 + (std::f32::consts::PI * x).cos());
        let wake = smooth((front - d) / 0.7);
        let body = (crest + PULSE_WAKE * wake).min(1.0);

        // Even, and one colour. This is the announcement — "here it is" — and
        // an announcement wants to be read at a glance, not studied. Every
        // attempt to give it structure has cost it that: lobes across the
        // surface become a pinwheel, angular colour becomes a rainbow. The
        // motion and the colour live outside the card, on the rim, where they
        // are not competing with the picture.
        let a = (body * PULSE_PEAK * overall).mul_add(1.0, flash) * tone * scale;
        match accent.filter(|_| PULSE_TINT > 0.0) {
            Some(accent) => accent.flat(PULSE_TINT, a),
            None => white(a),
        }
    });
}

/// Fills a rounded rect with a colour that varies per pixel, via `color_at(u,
/// v)` in the rect's own `0.0..=1.0` space.
///
/// The rounding is exact and analytic — each row's left/right inset is the
/// real circular-arc formula for that height, not a sampled polygon — so the
/// grid's outer edge sits exactly on the card's own silhouette, corners
/// included, at any aspect ratio.
fn scanline_mesh(
    painter: &Painter,
    rect: Rect,
    radius: f32,
    mut color_at: impl FnMut(f32, f32) -> Color32,
) {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return;
    }
    let r = radius
        .min(rect.width() * 0.5)
        .min(rect.height() * 0.5)
        .max(0.0);

    // Horizontal inset at height `y` (0 at the top edge), from the corner
    // circles: zero along the flat middle, growing toward `r` at the very
    // top/bottom edge of a rounded corner.
    let inset_at = |y: f32| -> f32 {
        let from_edge = y.min(rect.height() - y);
        if from_edge >= r || r <= 0.0 {
            0.0
        } else {
            let dz = r - from_edge;
            r - (r * r - dz * dz).max(0.0).sqrt()
        }
    };

    // Rows are not evenly spaced. The inset is flat across the middle and
    // then turns hard within `r` of either end, so evenly spaced rows spend
    // almost all of themselves on the straight part and leave the corner
    // described by one or two — which makes the mesh's silhouette cut across
    // the true arc, and the light either falls short of the corner or spills
    // past it onto whatever is behind the card.
    let ys = corner_dense_rows(rect.height(), r);

    let mut mesh = Mesh::default();
    let mut idx = vec![[0u32; GRID_COLS + 1]; ys.len()];
    for (yi, row) in idx.iter_mut().enumerate() {
        let y = ys[yi];
        let v = y / rect.height();
        let inset = inset_at(y);
        let (x0, x1) = (rect.left() + inset, rect.right() - inset);
        for (xi, slot) in row.iter_mut().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let f = xi as f32 / GRID_COLS as f32;
            let x = x0 + (x1 - x0) * f;
            let u = (x - rect.left()) / rect.width();
            *slot = mesh.vertices.len() as u32;
            mesh.vertices.push(Vertex {
                pos: pos2(x, rect.top() + y),
                // `Pos2::ZERO` samples the font atlas's white pixel (every
                // egui texture reserves it) — how an untextured mesh still
                // gets a texture id the renderer accepts.
                uv: Pos2::ZERO,
                color: color_at(u, v),
            });
        }
    }
    for yi in 0..ys.len() - 1 {
        for xi in 0..GRID_COLS {
            let (a, b, c, d) = (
                idx[yi][xi],
                idx[yi][xi + 1],
                idx[yi + 1][xi + 1],
                idx[yi + 1][xi],
            );
            mesh.add_triangle(a, b, c);
            mesh.add_triangle(a, c, d);
        }
    }
    painter.add(Shape::mesh(mesh));
}

/// Row heights for a rounded rect of height `h` and corner radius `r`: packed
/// through the two curved bands, sparse across the straight middle.
fn corner_dense_rows(h: f32, r: f32) -> Vec<f32> {
    /// Steps through one corner's worth of curve.
    const CORNER: usize = 14;

    let r = r.min(h * 0.5);
    let mut ys = Vec::with_capacity(CORNER * 2 + GRID_ROWS + 1);
    let push = |n: usize, a: f32, b: f32, out: &mut Vec<f32>| {
        for i in 0..=n {
            if i == 0 && !out.is_empty() {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let f = i as f32 / n as f32;
            out.push(b.mul_add(f, a * (1.0 - f)));
        }
    };
    push(CORNER, 0.0, r, &mut ys);
    push(GRID_ROWS, r, h - r, &mut ys);
    push(CORNER, h - r, h, &mut ys);
    ys
}

// ---------------------------------------------------------------------------
// Rim: the border takes the light up and settles into an ambient edge.
// ---------------------------------------------------------------------------

/// When the surface light first properly reaches the border: the moment the
/// crest's half-maximum crosses it, not the moment its centre does.
fn contact_time() -> f32 {
    let target = 1.0 - PULSE_BAND * 0.5;
    // The front is monotonic in time, so twenty halvings put this well inside
    // a frame. Cheap enough to do per card per frame, and it keeps the timing
    // honest when the band or the easing is retuned.
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..20 {
        let mid = f32::midpoint(lo, hi);
        if front_at(mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    f32::midpoint(lo, hi) * PULSE_HIT
}

#[allow(clippy::too_many_arguments)]
fn rim(painter: &Painter, rect: Rect, radius: f32, s: f32, accent: Option<&Accent>, tone: f32) {
    let start = (contact_time() - RIM_LEAD).max(0.0);
    if s < start {
        return;
    }
    let fade_start = start + RIM_IGNITE + RIM_HOLD;
    // The flare eases back to the resting level over the same stretch the
    // rim took to come up, so an overshoot reads as the border absorbing an
    // impact rather than as a second, brighter event.
    let flare = 1.0 + (RIM_OVERSHOOT - 1.0) * (1.0 - ramp(s, start, start + RIM_IGNITE * 4.0));
    let level = ramp(s, start, start + RIM_IGNITE)
        * (1.0 - ramp(s, fade_start, fade_start + RIM_FADE))
        * flare;
    if level <= 0.004 {
        return;
    }

    let Some(tex) = rim_texture(painter.ctx(), rect.size(), radius, HALO_OUT, HALO_IN) else {
        return;
    };
    let alpha = level * RIM_PEAK * tone;

    // How far the halo currently reaches. Starts tight against the border and
    // blooms out to the baked width, so the light grows from the edge.
    let reach = HALO_OUT * 0.18f32.mul_add(1.0, 0.82 * ease_out((s - start) / RIM_GROW.max(0.01)));

    // The same clock the pulse ran on, from the same origin — so the lobes do
    // not restart at the border, they arrive there already pointing where they
    // were pointing a frame earlier. The structure never flattens and the
    // motion never stops: what the eye loses at the end is the light.
    let spin = lobe_phase(s);

    ring_mesh(
        painter, rect, &tex, HALO_OUT, reach, alpha, spin, RIM_TINT, accent,
    );

    // A hairline on the border itself, in the same rotating colours.
    //
    // The halo alone disappears over a photograph: it is broad and low by
    // construction, and a busy background has plenty of its own broad, low
    // variation to hide it in. A line does not compete with that — it is a
    // frequency nothing in a photograph has — so a couple of points of it
    // makes the whole treatment read, at no cost to the softness.
    if RIM_EDGE > 0.004
        && let Some(edge) = rim_texture(painter.ctx(), rect.size(), radius, EDGE_OUT, EDGE_IN)
    {
        ring_mesh(
            painter,
            rect,
            &edge,
            EDGE_OUT,
            EDGE_OUT,
            level * RIM_EDGE * tone,
            spin,
            EDGE_TINT,
            accent,
        );
    }
}

/// How far the edge light reaches either side of the border, in points.
///
/// Asymmetric on purpose. Outward it stops quickly, which is what keeps it
/// reading as an edge rather than a second halo. Inward it runs much further
/// and tapers the whole way, so the border has weight where it meets the
/// card and thins out as it crosses the picture instead of ending on a line
/// of its own.
const EDGE_OUT: f32 = 3.0;
const EDGE_IN: f32 = 9.0;

/// Draws one nine-sliced ring of light: a texture holding the whole
/// cross-section, stretched over a frame around the card, tinted per vertex by
/// where the lobes currently are.
///
/// Nine-slice so the ring can be drawn narrower than it was baked without
/// dragging the card-shaped hole in the middle of the texture along with it.
/// The middle band maps the texture's card region onto exactly the card,
/// pinning the inner edge to the border at every width; only the outer bands
/// compress, which tightens the outward falloff rather than cutting it off.
#[allow(clippy::too_many_arguments)]
fn ring_mesh(
    painter: &Painter,
    rect: Rect,
    tex: &TextureHandle,
    baked: f32,
    reach: f32,
    alpha: f32,
    spin: f32,
    tint: f32,
    accent: Option<&Accent>,
) {
    let (tw, th) = (rect.width() + baked * 2.0, rect.height() + baked * 2.0);
    let cols = slice_axis(
        rect.left(),
        rect.right(),
        reach,
        baked / tw,
        (baked + rect.width()) / tw,
    );
    let rows = slice_axis(
        rect.top(),
        rect.bottom(),
        reach,
        baked / th,
        (baked + rect.height()) / th,
    );
    let (half_w, half_h) = (rect.width() * 0.5, rect.height() * 0.5);
    let centre = rect.center();

    let mut mesh = Mesh::with_texture(tex.id());
    let mut idx = vec![vec![0u32; cols.len()]; rows.len()];
    for (yi, &(y, v)) in rows.iter().enumerate() {
        for (xi, &(x, u)) in cols.iter().enumerate() {
            let theta = aspect_angle((x - centre.x) / half_w, (y - centre.y) / half_h);
            // Peaks hold at the given alpha; depth only says how far the
            // troughs between them fall away.
            let a = alpha * lobe_at(theta, spin, RIM_LOBES, RIM_LOBE_DEPTH);
            idx[yi][xi] = mesh.vertices.len() as u32;
            mesh.vertices.push(Vertex {
                pos: pos2(x, y),
                uv: pos2(u, v),
                color: match accent.filter(|_| tint > 0.0) {
                    Some(accent) => accent.at(theta, spin, tint, a),
                    None => white(a),
                },
            });
        }
    }
    for yi in 0..rows.len() - 1 {
        for xi in 0..cols.len() - 1 {
            let (a, b, c, d) = (
                idx[yi][xi],
                idx[yi][xi + 1],
                idx[yi + 1][xi + 1],
                idx[yi + 1][xi],
            );
            mesh.add_triangle(a, b, c);
            mesh.add_triangle(a, c, d);
        }
    }
    painter.add(Shape::mesh(mesh));
}

/// One axis of the nine-slice: sample positions paired with the texture
/// coordinate each one reads.
///
/// The outer bands span `reach` points but always cover the texture's full
/// margin, which is what lets the halo be drawn at any width from a single
/// baked image. Both are subdivided, densely enough that the lobe pattern
/// carried in vertex colour has somewhere to live.
fn slice_axis(lo: f32, hi: f32, reach: f32, u_lo: f32, u_hi: f32) -> Vec<(f32, f32)> {
    /// Steps across each outer margin, and across the card itself.
    const EDGE: usize = 6;
    const MID: usize = 16;

    let mut out = Vec::with_capacity(EDGE * 2 + MID + 1);
    let mut push = |n: usize, p0: f32, p1: f32, t0: f32, t1: f32| {
        for i in 0..=n {
            if i == 0 && !out.is_empty() {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let f = i as f32 / n as f32;
            out.push((p1.mul_add(f, p0 * (1.0 - f)), t1.mul_add(f, t0 * (1.0 - f))));
        }
    };
    push(EDGE, lo - reach, lo, 0.0, u_lo);
    push(MID, lo, hi, u_lo, u_hi);
    push(EDGE, hi, hi + reach, u_hi, 1.0);
    out
}

/// The few colours a capture is actually made of, for lighting its own rim.
///
/// Sampled once when the thumbnail is uploaded — the pixels are gone by the
/// time anything paints, so this has to be taken while the image is still in
/// hand — and carried on [`CardContent`](crate::card::CardContent).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Accent {
    /// Three dominant hues, in the order they should travel around the rim.
    colors: [Color32; 3],
    /// How colourful the capture was at all, `0.0..=1.0`. A screenshot of a
    /// text editor is nearly grey, and its rim should stay white rather than
    /// have a colour invented for it.
    strength: f32,
    /// Mean brightness of the capture, `0.0..=1.0`.
    luma: f32,
}

impl Accent {
    /// How colourful the capture was, `0.0..=1.0`.
    ///
    /// Exposed so the "a grey screenshot keeps a white rim" rule is testable
    /// without rendering one and eyeballing the result.
    #[must_use]
    pub const fn strength(&self) -> f32 {
        self.strength
    }

    /// What to scale the surface pulse by, given how bright the capture is.
    ///
    /// The first version of this chased equal *perceived brightness*: it
    /// divided by the headroom above the capture, so a near-white one got
    /// several times the alpha. That is the wrong target. A white wash over a
    /// light screenshot does not just brighten it, it lifts the blacks — the
    /// dark text is the only contrast such a capture has, and washing it out
    /// erases the picture rather than lighting it.
    ///
    /// So the curve rises gently and stops early. Dark captures are damped,
    /// because white on near-black is a violent change; light ones get a
    /// little more than mid-grey, and no more than that, because past a point
    /// the extra alpha costs legibility instead of buying visibility.
    fn surface_scale(&self) -> f32 {
        0.5f32.mul_add(smooth(self.luma / 0.85), 0.55)
    }

    /// One colour for the whole surface: the three sampled hues averaged, then
    /// mixed back toward white.
    ///
    /// The rim varies its colour by angle, which is what makes it read as
    /// light moving around the card. Doing the same across the thumbnail
    /// paints a rainbow over the picture — every hue on screen at once, on the
    /// one surface the user is actually trying to look at. Inside the card the
    /// light gets a single tone or none.
    fn flat(&self, tint: f32, alpha: f32) -> Color32 {
        let mix = (tint * self.strength).clamp(0.0, 1.0);
        let alpha = alpha.clamp(0.0, 1.0);
        let chan = |pick: fn(&Color32) -> u8| -> u8 {
            let avg = self.colors.iter().map(|c| f32::from(pick(c))).sum::<f32>() / 3.0;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                (255.0f32.mul_add(1.0 - mix, avg * mix) * alpha) as u8
            }
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Color32::from_rgba_premultiplied(
            chan(Color32::r),
            chan(Color32::g),
            chan(Color32::b),
            (alpha * 255.0) as u8,
        )
    }

    /// The colour at a point on the rim: the three sampled hues laid out
    /// around the border and drifting with the lobes, mixed back toward white
    /// by however little colour the capture had to give.
    fn at(&self, theta: f32, spin: f32, tint: f32, alpha: f32) -> Color32 {
        let u = (theta - spin) / std::f32::consts::TAU;
        let u = u.rem_euclid(1.0) * 3.0;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let i = u as usize % 3;
        let f = smooth(u.fract());
        let (a, b) = (self.colors[i], self.colors[(i + 1) % 3]);

        let mix = (tint * self.strength).clamp(0.0, 1.0);
        let alpha = alpha.clamp(0.0, 1.0);
        let chan = |ca: u8, cb: u8| -> u8 {
            let hue = f32::from(cb).mul_add(f, f32::from(ca) * (1.0 - f));
            // Toward white, not toward black: this is light being coloured,
            // so the least tinted version of it is the brightest one.
            let lit = 255.0f32.mul_add(1.0 - mix, hue * mix);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                (lit * alpha) as u8
            }
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Color32::from_rgba_premultiplied(
            chan(a.r(), b.r()),
            chan(a.g(), b.g()),
            chan(a.b(), b.b()),
            (alpha * 255.0) as u8,
        )
    }
}

/// How many hue buckets the sampler sorts a capture into. Coarse on purpose:
/// this is looking for what the screenshot is *made of*, not for detail.
const HUE_BINS: usize = 24;

/// Samples a thumbnail for the colours its rim should be lit in.
///
/// Weighted by saturation and brightness, so a mostly-white document with one
/// blue toolbar comes back blue rather than coming back white — the colour
/// that is *present* matters more than the colour that covers the most area.
/// A capture with no colour in it at all reports a strength near zero and the
/// rim stays monochrome.
#[must_use]
pub fn sample_accent(image: &ColorImage) -> Accent {
    // Enough samples to be stable, few enough to be free: this runs once per
    // capture, on the frame its thumbnail is uploaded.
    const TARGET_SAMPLES: usize = 4096;

    let mut weight = [0.0f32; HUE_BINS];
    let mut sat = [0.0f32; HUE_BINS];
    let mut total = 0.0f32;
    let mut counted = 0.0f32;
    let mut brightness = 0.0f32;

    let stride = (image.pixels.len() / TARGET_SAMPLES).max(1);
    for px in image.pixels.iter().step_by(stride) {
        let [r, g, b, a] = px.to_srgba_unmultiplied();
        if a <= 8 {
            continue;
        }
        let coverage = f32::from(a) / 255.0;
        let visible = Color32::from_rgb(r, g, b);
        let hsva = egui::ecolor::Hsva::from(visible);
        // Very dark and very washed-out pixels carry no usable hue, and
        // letting them vote drags every capture toward the same muddy result.
        let w = hsva.s * hsva.v.powf(0.6) * coverage;
        counted += coverage;
        // Rec. 709 weights, in gamma space — the question is how bright this
        // looks, not how much light it emits.
        brightness += 0.2126f32.mul_add(
            f32::from(r),
            0.7152f32.mul_add(f32::from(g), 0.0722 * f32::from(b)),
        ) / 255.0
            * coverage;
        if w <= 0.02 {
            continue;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bin = ((hsva.h.rem_euclid(1.0) * HUE_BINS as f32) as usize).min(HUE_BINS - 1);
        weight[bin] += w;
        sat[bin] += hsva.s * w;
        total += w;
    }

    let mut picked = [Color32::WHITE; 3];
    let mut taken = 0usize;
    let mut used = [false; HUE_BINS];
    while taken < 3 {
        let best = (0..HUE_BINS)
            .filter(|&b| !used[b] && weight[b] > 0.0)
            .max_by(|&a, &b| weight[a].total_cmp(&weight[b]));
        let Some(bin) = best else { break };
        // Claim the neighbours too, so three picks are three *different*
        // colours rather than three slices of one gradient.
        for offset in 0..=2 {
            used[(bin + offset) % HUE_BINS] = true;
            used[(bin + HUE_BINS - offset) % HUE_BINS] = true;
        }
        #[allow(clippy::cast_precision_loss)]
        let hue = (bin as f32 + 0.5) / HUE_BINS as f32;
        // Under the sampled saturation, but not by much. Held too far below
        // it — and then mixed toward white on top of that — the colour
        // survives the arithmetic in name only and every capture lights its
        // rim the same off-white.
        let s = (sat[bin] / weight[bin] * 0.95).clamp(0.35, 0.9);
        picked[taken] = egui::ecolor::Hsva::new(hue, s, 1.0, 1.0).into();
        taken += 1;
    }
    // Fewer than three distinct hues: repeat what there is rather than
    // padding with white, which would read as a gap in the ring.
    for i in taken..3 {
        picked[i] = if taken == 0 {
            Color32::WHITE
        } else {
            picked[i % taken.max(1)]
        };
    }

    // The average of `s * v` over a whole screenshot is small even for a
    // colourful one — most of any interface is grey chrome — so this needs
    // real gain to reach 1.0 on a capture a person would call colourful. At
    // the old gain almost everything landed near 0.3, and since this scales
    // the mix toward white, almost everything came out white.
    let luma = if counted > 0.0 {
        (brightness / counted).clamp(0.0, 1.0)
    } else {
        0.5
    };
    let strength = if counted > 0.0 {
        (total / counted * 8.0).clamp(0.0, 1.0)
    } else {
        0.0
    };
    Accent {
        colors: picked,
        strength,
        luma,
    }
}

/// Cached by rounded card size, radius and halo reach. A handful of entries
/// at most — the stack's cards are all one size.
type RimKey = (u32, u32, u32, u32, u32);

#[derive(Clone, Default)]
struct RimTextureCache {
    textures: HashMap<RimKey, TextureHandle>,
}

const RIM_CACHE_ID: &str = "scrozz-card-landing-rim-cache";

/// Bakes the rim's cross-section: an alpha image of the card's rounded-rect
/// silhouette expanded by `halo_out`, every texel holding the gaussian of its
/// own distance to the border.
///
/// This is the whole reason the effect is smooth. The falloff is evaluated
/// per texel and filtered by the sampler, so there is no triangle anywhere in
/// it to show an edge, at any size.
fn rim_texture(
    ctx: &egui::Context,
    size: egui::Vec2,
    radius: f32,
    halo_out: f32,
    halo_in: f32,
) -> Option<TextureHandle> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let key = (
        size.x.round() as u32,
        size.y.round() as u32,
        radius.round() as u32,
        halo_out.round() as u32,
        halo_in.round() as u32,
    );
    let cache_id = egui::Id::new(RIM_CACHE_ID);
    if let Some(handle) = ctx.data(|data| {
        data.get_temp::<RimTextureCache>(cache_id)
            .and_then(|cache| cache.textures.get(&key).cloned())
    }) {
        return Some(handle);
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (w, h) = (
        (size.x + halo_out * 2.0).round().max(4.0) as usize,
        (size.y + halo_out * 2.0).round().max(4.0) as usize,
    );
    if w > 2048 || h > 2048 {
        return None;
    }

    let (half_w, half_h) = (size.x * 0.5, size.y * 0.5);
    let r = radius.min(half_w).min(half_h).max(0.0);
    // Standard exact rounded-rect signed distance: distance to the inset
    // rectangle's corner box, less the corner radius. Positive outside the
    // border, negative inside it, and correct through the corners.
    let sdf = |px: f32, py: f32| -> f32 {
        let qx = (px - half_w).abs() - (half_w - r);
        let qy = (py - half_h).abs() - (half_h - r);
        let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
        outside + qx.max(qy).min(0.0) - r
    };

    // Not a gaussian. A gaussian keeps most of its mass close in and then
    // stops — widening it just makes the bright collar fatter, which is
    // exactly the wrong half of "wider and softer". This falls away from the
    // border immediately and reaches true zero at the full reach, so the
    // light is thin everywhere and simply goes further: a cast shadow rather
    // than an outline.
    let taper_out = 2.4;
    let taper_in = 1.8;

    let mut pixels = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            #[allow(clippy::cast_precision_loss)]
            let (px, py) = (x as f32 + 0.5 - halo_out, y as f32 + 0.5 - halo_out);
            let d = sdf(px, py);
            let (reach, taper) = if d >= 0.0 {
                (halo_out, taper_out)
            } else {
                (halo_in, taper_in)
            };
            // Both branches are 1.0 at the border itself, so the profile is
            // continuous across it and there is no seam on the card's edge.
            let z = (d.abs() / reach).min(1.0);
            pixels.push(white((1.0 - z).powf(taper)));
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let image = ColorImage {
        size: [w, h],
        source_size: egui::Vec2::new(w as f32, h as f32),
        pixels,
    };
    let handle = ctx.load_texture(
        format!(
            "landing-rim:{}x{}:{}:{}:{}",
            key.0, key.1, key.2, key.3, key.4
        ),
        image,
        TextureOptions::LINEAR,
    );
    // Texture handles belong to one egui texture manager. Keeping this cache
    // in Context data prevents another editor, harness render, or test Context
    // from reusing an identically numbered handle owned elsewhere.
    ctx.data_mut(|data| {
        let mut cache = data
            .get_temp::<RimTextureCache>(cache_id)
            .unwrap_or_default();
        // One entry per card size per recipe; the cap only guards against a
        // pathological resize loop interning unbounded textures.
        if cache.textures.len() >= 16 {
            cache.textures.clear();
        }
        cache.textures.insert(key, handle.clone());
        data.insert_temp(cache_id, cache);
    });
    Some(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(pixels: Vec<Color32>) -> ColorImage {
        ColorImage {
            size: [pixels.len(), 1],
            source_size: egui::vec2(pixels.len() as f32, 1.0),
            pixels,
        }
    }

    #[test]
    fn invisible_rgb_cannot_tint_or_dim_a_capture_accent() {
        let grey = Color32::from_gray(160);
        let visible = sample_accent(&image(vec![grey; 4]));
        let mut pixels = vec![grey; 4];
        pixels.extend(std::iter::repeat_n(
            Color32::from_rgba_unmultiplied(255, 0, 0, 0),
            4_092,
        ));
        let with_invisible_red = sample_accent(&image(pixels));

        assert_eq!(with_invisible_red.strength, visible.strength);
        assert!((with_invisible_red.luma - visible.luma).abs() < f32::EPSILON);
    }

    #[test]
    fn rim_textures_are_cached_per_egui_context() {
        let first = egui::Context::default();
        let second = egui::Context::default();
        let cache_id = egui::Id::new(RIM_CACHE_ID);
        let cache_len = |ctx: &egui::Context| {
            ctx.data(|data| {
                data.get_temp::<RimTextureCache>(cache_id)
                    .map_or(0, |cache| cache.textures.len())
            })
        };

        assert_eq!(cache_len(&first), 0);
        assert_eq!(cache_len(&second), 0);
        assert!(rim_texture(&first, egui::vec2(210.0, 150.0), 12.0, 54.0, 11.0).is_some());
        assert_eq!(cache_len(&first), 1);
        assert_eq!(cache_len(&second), 0);
        assert!(rim_texture(&second, egui::vec2(210.0, 150.0), 12.0, 54.0, 11.0).is_some());
        assert_eq!(cache_len(&second), 1);
    }
}
