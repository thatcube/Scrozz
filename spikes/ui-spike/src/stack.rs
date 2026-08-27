//! The **live** capture stack — the interactive counterpart to the static
//! `QuickVariant` scenes in `surfaces.rs`.
//!
//! Everything the D19 motion baseline asks for happens here: hover reveal with
//! stagger, press lift, drag-with-lag, velocity-based swipe-to-dismiss with
//! tilt and momentum, and a new capture entering the deck while the cards
//! beneath settle back on a spring.
//!
//! Two different mechanisms drive it, deliberately:
//!
//! * **Timeline** animations (`motion::anim`, `motion::stagger`) for anything
//!   with a known start and end — hover chrome, button states. egui schedules
//!   its own repaints for these and stops when they settle.
//! * **Springs** (`motion::Spring1/2`) for anything interruptible or physical —
//!   deck depth, drag follow, fling coast. These do *not* self-schedule, so
//!   [`Stack::show`] returns whether any of them is still moving and the app
//!   turns that into a `request_repaint`.

use crate::icons::IconStore;
use crate::motion::{self, Spring1, Spring2};
use crate::paint;
use crate::theme::{self, cr, Palette};
use egui::{pos2, vec2, Align2, Color32, Id, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};

pub const CARD_W: f32 = 252.0;
pub const CARD_H: f32 = 157.0;
/// Gap between cards in the list. From the spacing scale — not a new number.
const GAP: f32 = theme::SP_3;
/// Centre-to-centre distance between two slots in the list.
const SLOT: f32 = CARD_H + GAP;
/// How many captures stay on screen. Beyond this the oldest fades out as it is
/// pushed past the end of the list.
///
/// Four is the choice: it fills a laptop-height overlay column without the list
/// running off the top of the screen, and it is enough captures to be useful
/// while every one of them stays directly clickable.
// Six is the practical ceiling on a 16-inch MacBook Pro, and matches what
// comparable tools allow. In the real app this is derived from the display's
// work-area height (D28); the spike hard-codes it because its scene is a
// window rather than a screen.
pub const MAX_VISIBLE: usize = 6;
/// How long the "Copied" / "Dismissed" confirmation stays up, in seconds.
const TOAST_LIFE: f32 = 1.25;

/// Which screen edge the overlay is docked to.
///
/// CleanShot's setting is literally "Position on screen: Left". Cards **enter
/// from, and exit toward, the anchored edge** — so the whole gesture language
/// is one signed axis rather than a hardcoded direction. Left is the default
/// and the case that has to be excellent.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Anchor {
    #[default]
    Left,
    Right,
}

impl Anchor {
    /// +1 if "away" means increasing x, −1 if it means decreasing x.
    pub fn sign(self) -> f32 {
        match self {
            Anchor::Left => -1.0,
            Anchor::Right => 1.0,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Anchor::Left => "left",
            Anchor::Right => "right",
        }
    }
    pub fn flipped(self) -> Self {
        match self {
            Anchor::Left => Anchor::Right,
            Anchor::Right => Anchor::Left,
        }
    }
}

/// How far off-screen a card starts, and how far it must travel to be gone.
///
/// Has to clear the *window*, not just the card: slot 0 sits ~30px inside the
/// docked edge, so a card only reads as arriving from off-screen if it starts
/// its own width plus that inset beyond the frame.
const OFFSCREEN: f32 = CARD_W + 260.0;

pub struct Card {
    pub id: u64,
    variant: usize,
    name: String,
    dims: &'static str,
    /// Animated position in the deck. Target is the card's index, so inserting
    /// or removing a card makes every other card spring to its new slot.
    depth: Spring1,
    /// Offset from the card's home position. **Snapped 1:1 to the pointer while
    /// held**, springs home on a cancelled drag, coasts ballistically once flung.
    drag: Spring2,
    drag_target: Vec2,
    /// Counts down before this card's depth spring starts moving, so the deck
    /// cascades instead of shifting as one rigid body.
    settle_delay: f32,
    angle: f32,
    spin: f32,
    alpha: f32,
    /// 0→1 entry ramp. Only used for the entry tilt now that entry is a real
    /// slide rather than a fade.
    entry: f32,
}

impl Card {
    fn new(id: u64, variant: usize, name: String, dims: &'static str, index: f32) -> Self {
        Self {
            id,
            variant,
            name,
            dims,
            depth: Spring1::at(index),
            drag: Spring2::at(Vec2::ZERO),
            drag_target: Vec2::ZERO,
            settle_delay: 0.0,
            angle: 0.0,
            spin: 0.0,
            alpha: 1.0,
            entry: 1.0,
        }
    }

    /// Seed the "just captured" state: fully off-screen past the anchored edge,
    /// already moving inward. The settle spring carries it home from there.
    ///
    /// This is a **slide**, not a fade — the card is opaque the whole way in.
    /// A capture that fades in looks like a notification; one that slides in
    /// from the edge it lives on looks like an object arriving.
    ///
    /// `slot` is the card's destination height, and depth starts *there* rather
    /// than at zero. That is what makes entry purely horizontal: the card
    /// arrives at the top of the pile travelling straight in from the side,
    /// instead of rising from the bottom and shoving the deck upward. Per D28 a
    /// card must never move upward once resident.
    fn seed_entry(&mut self, anchor: Anchor, slot: f32) {
        let s = anchor.sign();
        self.depth = Spring1::at(slot);
        self.drag = Spring2 {
            pos: vec2(s * OFFSCREEN, 0.0),
            // A little inward launch speed so it reads as thrown in rather than
            // merely pulled by a spring.
            vel: vec2(-s * 420.0, 0.0),
        };
        self.drag_target = Vec2::ZERO;
        self.settle_delay = 0.0;
        self.entry = 0.0;
        self.alpha = 1.0;
        self.angle = 0.0;
        self.spin = 0.0;
    }

    fn advance_entry(&mut self, dt: f32) -> bool {
        if self.entry >= 1.0 {
            return false;
        }
        let d = motion::dur(motion::SLOW);
        if d <= 0.0 {
            self.entry = 1.0; // reduce-motion: no ramp at all
            return false;
        }
        self.entry = (self.entry + dt / d).min(1.0);
        true
    }
}

pub struct Stack {
    deck: Vec<Card>,
    flying: Vec<Card>,
    next_id: u64,
    clock: u32,
    /// Bumped by Replay so every keyed animation restarts from scratch.
    pub epoch: u64,
    toast: Option<(String, f32)>,
    /// Which edge cards arrive from and leave toward.
    pub anchor: Anchor,
    /// Real pointer velocity over the last few frames, carried into the fling.
    vel: motion::VelocityTracker,
    /// Id of the card the pointer is currently holding, if any. **Any** card in
    /// the list is grabbable, not just the newest one.
    dragging: Option<u64>,
}

fn label(clock: u32) -> (String, &'static str) {
    let dims = ["2048 × 1280", "1920 × 1080", "1440 × 900", "2560 × 1600"][(clock % 4) as usize];
    let m = 28 + (clock * 7) % 32;
    let s = (clock * 23) % 60;
    (format!("Screen 14.{m:02}.{s:02}"), dims)
}

impl Default for Stack {
    fn default() -> Self {
        Self::new()
    }
}

impl Stack {
    pub fn new() -> Self {
        let mut s = Self {
            deck: Vec::new(),
            flying: Vec::new(),
            next_id: 0,
            clock: 0,
            epoch: 0,
            toast: None,
            anchor: Anchor::default(),
            vel: motion::VelocityTracker::default(),
            dragging: None,
        };
        for i in 0..MAX_VISIBLE {
            let (name, dims) = label(s.clock);
            s.clock += 1;
            let id = s.next_id;
            s.next_id += 1;
            s.deck.push(Card::new(id, i, name, dims, i as f32));
        }
        s
    }

    /// `N` — a new capture slides in from the anchored edge, **on top of the
    /// pile**, and the oldest leaves from the bottom when there is no room.
    ///
    /// Index 0 is the bottom slot, so the newest capture is the *last* element.
    /// This is D28: the pile is anchored at the bottom and grows upward, the
    /// newest arrives on top, the oldest exits leftward from the bottom, and
    /// everything above a departure falls **down** into the gap. A card never
    /// moves upward.
    pub fn spawn(&mut self) {
        let (name, dims) = label(self.clock);
        self.clock += 1;
        let id = self.next_id;
        self.next_id += 1;

        // Overflow first, so the arriving card's destination slot is correct
        // and the fall and the exit read as one settling motion rather than two.
        if self.deck.len() >= MAX_VISIBLE {
            self.evict_oldest();
        }

        let slot = self.deck.len() as f32;
        let mut c = Card::new(id, (id as usize + 1) % 4, name, dims, slot);
        c.seed_entry(self.anchor, slot);
        self.deck.push(c);
        // Deliberately no `stagger_deck()` here. While the pile is merely
        // growing, D28 says the resident cards do not move at all, so there is
        // nothing to ripple — and staggering would give the *arriving* card the
        // largest delay, making the newest capture the slowest to appear.
        // The ripple belongs to eviction, and `evict_oldest` applies it there.
        self.toast = Some(("Captured".into(), 0.0));
    }

    /// Retire the bottom-most (oldest) card leftward, exactly as a manual
    /// dismissal would. Overflow is not a special effect: the card leaves the
    /// same way it arrived and the same way the user could have sent it.
    fn evict_oldest(&mut self) {
        if self.deck.is_empty() {
            return;
        }
        let s = self.anchor.sign();
        let mut c = self.deck.remove(0);
        c.drag.vel = vec2(s * 1150.0, -60.0);
        c.spin = s * 1.1;
        self.flying.push(c);
        // No explicit re-target needed: the physics pass drives each card's
        // depth spring toward its current list index, so removing element 0
        // makes every card above it fall exactly one slot, smoothly.
        //
        // Stagger the fall from the bottom upward: the card nearest the gap
        // starts first and the ripple travels up the pile. That reads as a
        // stack settling under gravity rather than a rigid block sliding.
        self.stagger_deck();
    }

    /// Give each card below the front a slightly later start, so the reflow
    /// ripples down the deck. Cheap, and it is the whole difference between a
    /// stack that feels like objects and one that feels like a rigid diagram.
    fn stagger_deck(&mut self) {
        let step = motion::settle_stagger();
        for (i, c) in self.deck.iter_mut().enumerate() {
            if i > 0 {
                c.settle_delay = step * i as f32;
            }
        }
    }

    /// `Backspace` — throw the newest card toward the anchored edge without a
    /// gesture, so the dismiss motion is reachable from the keyboard.
    ///
    /// "Top" is the visual top of the pile, which is the *last* element now
    /// that index 0 is the bottom slot.
    pub fn dismiss_top(&mut self) {
        if self.deck.is_empty() {
            return;
        }
        let s = self.anchor.sign();
        let mut c = self.deck.pop().expect("deck is non-empty");
        c.drag.vel = vec2(s * 1150.0, -60.0);
        c.spin = s * 1.1;
        self.flying.push(c);
        self.stagger_deck();
    }

    /// `R` / tuner Replay — re-run the entry slide for the whole deck so the
    /// maintainer can watch the same motion repeatedly without restarting.
    pub fn replay(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.flying.clear();
        self.dragging = None;
        self.vel.clear();
        let s = self.anchor.sign();
        let step = motion::settle_stagger();
        for (i, c) in self.deck.iter_mut().enumerate() {
            c.depth = Spring1::at(i as f32);
            c.drag = Spring2 {
                pos: vec2(s * (OFFSCREEN + i as f32 * 40.0), 0.0),
                vel: vec2(-s * 420.0, 0.0),
            };
            c.drag_target = Vec2::ZERO;
            c.settle_delay = step * i as f32;
            c.entry = 0.0;
            c.alpha = 1.0;
            c.angle = 0.0;
            c.spin = 0.0;
        }
    }

    /// A compact numeric fingerprint of every animated value in the stack.
    ///
    /// Exists so headless tests can assert that motion is *actually happening*
    /// rather than that a spring is ticking behind a static picture. Cheap, and
    /// the only alternative — counting emitted shapes — needs epaint internals
    /// that aren't public.
    #[allow(dead_code)] // test-only observer
    pub fn pose(&self) -> Vec<f32> {
        let mut v = Vec::with_capacity((self.deck.len() + self.flying.len()) * 6);
        for c in self.deck.iter().chain(self.flying.iter()) {
            v.extend_from_slice(&[
                c.depth.pos,
                c.drag.pos.x,
                c.drag.pos.y,
                c.angle,
                c.alpha,
                c.entry,
            ]);
        }
        v
    }

    /// Number of cards currently in the deck (excludes ones flying away).
    #[allow(dead_code)] // test-only observer
    pub fn len(&self) -> usize {
        self.deck.len()
    }

    /// Geometry for a card at fractional list position `d`.
    ///
    /// **A vertical list, not a stack.** Every card is the same size at full
    /// opacity with a real gap between it and its neighbours; the only thing
    /// `d` changes is which slot it occupies. Slot 0 sits at the anchored
    /// corner and older captures are pushed away from it. Fractional `d` is
    /// what makes reflow continuous, so a card moving from slot 2 to slot 1
    /// slides rather than jumps.
    pub fn geom(home: Rect, d: f32) -> Rect {
        Rect::from_min_size(home.min + vec2(0.0, -SLOT * d), home.size())
    }

    /// Cards fade only once they are pushed *past* the end of the list. Inside
    /// the list there is no opacity falloff at all — every capture is fully
    /// visible and directly actionable, which was the whole point of dropping
    /// the stack metaphor.
    pub fn slot_alpha(d: f32) -> f32 {
        (1.0 - (d - (MAX_VISIBLE as f32 - 1.0))).clamp(0.0, 1.0)
    }

    /// Draw + drive one frame. Returns `true` while any spring is still moving,
    /// which the app turns into a `request_repaint` — the one thing that has to
    /// be right for hand-rolled physics to animate at all in immediate mode.
    pub fn show(&mut self, ui: &mut Ui, icons: &IconStore, pal: &Palette, scene: Rect) -> bool {
        let ctx = ui.ctx().clone();
        let dt = motion::dt_of(&ctx);
        let rt = theme::R_THUMB;
        let mut active = false;

        // The overlay is docked to the anchored edge, so slot 0 sits *on* that
        // edge — cards enter from, and exit toward, the side they live on.
        let hx = match self.anchor {
            Anchor::Left => scene.left() + 30.0,
            Anchor::Right => scene.right() - 30.0 - CARD_W,
        };
        let home =
            Rect::from_min_size(pos2(hx, scene.bottom() - 34.0 - CARD_H), vec2(CARD_W, CARD_H));

        // ---- physics -------------------------------------------------------
        let held_id = self.dragging;
        for (i, c) in self.deck.iter_mut().enumerate() {
            // Stagger: a card waits its turn before starting to settle.
            if c.settle_delay > 0.0 {
                c.settle_delay = (c.settle_delay - dt).max(0.0);
                if !motion::reduced() {
                    active = true;
                }
            } else {
                active |= c.depth.step(i as f32, dt, motion::deck_k(), motion::deck_c());
            }

            if held_id == Some(c.id) {
                // **1:1 with the pointer.** No spring, no lag: a card being
                // dragged must sit exactly under the finger or the gesture
                // feels like it is fighting you. Inertia belongs to the
                // *release*, not the hold.
                c.drag.pos = c.drag_target;
                c.drag.vel = Vec2::ZERO;
            } else {
                active |= c.drag.step(
                    c.drag_target,
                    dt,
                    motion::settle_k(),
                    motion::settle_c(),
                );
            }
            active |= c.advance_entry(dt);

            // Tilt tracks lateral offset *and* velocity, so the card leans into
            // a throw before it has travelled far. Entry adds a slight tilt
            // that unwinds as the card lands.
            let lean = if held_id == Some(c.id) {
                c.drag_target.x * 0.0016
            } else {
                c.drag.pos.x * 0.0016 + c.drag.vel.x * 0.00008
            };
            c.angle = lean.clamp(-0.26, 0.26) + 0.07 * (1.0 - c.entry) * self.anchor.sign();
        }
        // Reap a card once it has finished being pushed off the end of the list.
        while self.deck.len() > MAX_VISIBLE
            && self.deck.last().is_some_and(|c| c.depth.pos >= MAX_VISIBLE as f32 - 0.02)
        {
            self.deck.pop();
        }
        for c in self.flying.iter_mut() {
            // Light gravity: a dismissed card should exit *sideways past the
            // edge it belongs to*, not arc into the floor.
            c.drag.coast(dt, motion::fling_drag(), vec2(0.0, 260.0));
            c.angle += c.spin * dt;
            // Fade over distance travelled, not over wall-clock time, so a fast
            // flick stays solid until it is genuinely gone and a weak one
            // dissolves near the edge.
            let travelled = (c.drag.pos.x * self.anchor.sign()).max(0.0);
            let by_dist = 1.0 - (travelled / OFFSCREEN).clamp(0.0, 1.0);
            let fade_floor = if motion::reduced() { 0.0 } else { c.alpha - dt * 0.8 };
            c.alpha = by_dist.min(c.alpha).min(fade_floor.max(0.0)).max(0.0);
            active = true;
        }
        self.flying.retain(|c| c.alpha > 0.01);
        if let Some((_, age)) = &mut self.toast {
            *age += dt;
            if *age > TOAST_LIFE {
                self.toast = None;
            } else if motion::reduced() {
                // Reduce-motion draws the toast flat, so there is nothing to
                // animate — but it still has to disappear. Sleep until it
                // expires instead of asking for 75 identical frames. This is
                // what `request_repaint_after` is for, and it is the difference
                // between a dwell timer and a busy-wait.
                ctx.request_repaint_after(std::time::Duration::from_secs_f32(
                    (TOAST_LIFE - *age).max(0.0),
                ));
            } else {
                active = true;
            }
        }

        // ---- the list -------------------------------------------------------
        //
        // A vertical list of discrete cards, drawn furthest-slot-first so that
        // whichever card is being dragged ends up on top. **Every card gets the
        // full treatment** — its own hover reveal, its own grab, its own fling.
        // Nothing here special-cases the newest card any more.
        let p = ui.painter().clone();
        let epoch = self.epoch;
        let anchor = self.anchor;
        let mut fling: Option<(usize, Vec2)> = None;
        let mut clicked: Option<&'static str> = None;
        let count = self.deck.len();

        let mut order: Vec<usize> = (0..self.deck.len()).rev().collect();
        if let Some(at) = self
            .dragging
            .and_then(|id| self.deck.iter().position(|c| c.id == id))
        {
            order.retain(|&i| i != at);
            order.push(at);
        }

        for slot in order {
            let Stack { deck, vel, dragging, .. } = self;
            let card = &mut deck[slot];
            let list_a = Self::slot_alpha(card.depth.pos);
            if list_a <= 0.004 {
                continue;
            }
            let base = Self::geom(home, card.depth.pos).translate(card.drag.pos);
            let cid = Id::new(("scrozz-card", card.id, epoch));
            let resp = ui.interact(base, cid, Sense::click_and_drag());

            let held = resp.is_pointer_button_down_on() && !paint::shot_mode();
            // Geometric hover, *not* `Response::hovered()`: the chrome buttons
            // are registered above the card, so `hovered()` goes false the
            // instant the pointer crosses onto Copy — which would collapse the
            // reveal you just moved into.
            let over = ui.rect_contains_pointer(base) && !paint::shot_mode();
            let moved = card.drag.pos.length() > 5.0;
            let reveal_on = over && !moved;

            // Press lift: a small overshoot so the card feels picked up.
            let lift =
                motion::anim(&ctx, cid.with("lift"), held, motion::FAST, motion::spring_overshoot);
            let rect = base.expand(3.0 * lift + if moved { 4.0 } else { 0.0 });

            paint::soft_shadow(&p, rect.shrink(4.0), rt, pal, list_a * (0.7 + 0.5 * lift));
            paint::capture_face(
                &p,
                rect,
                rt,
                card.angle,
                motion::alpha(255, card.alpha * list_a * card.entry.max(0.15)),
                card.variant,
                1.0 + 0.9 * lift + if moved { 0.8 } else { 0.0 },
            );

            // The reveal itself: scrim first, then the six controls cascading.
            let scrim = motion::anim(
                &ctx,
                cid.with(("scrim", epoch)),
                reveal_on,
                motion::FAST,
                motion::ease_out_cubic,
            );
            let sg = motion::stagger(
                &ctx,
                cid.with(("chrome", epoch)),
                reveal_on,
                6,
                motion::BASE,
                motion::STEP,
                motion::ease_out_cubic,
            );
            // Tilting hides the chrome entirely — rotated text is not
            // achievable in epaint, so the chrome must be gone before the card
            // leans far enough for upright labels to look wrong.
            let flat = (1.0 - (card.angle.abs() / 0.10)).clamp(0.0, 1.0) * list_a;
            let scrim = scrim * flat;
            let inset = 9.0;
            let d = 27.0;

            if scrim > 0.004 {
                p.rect_filled(rect, cr(rt), Color32::from_black_alpha(motion::alpha(96, scrim)));
                paint::bottom_scrim(&p, rect, 46.0, rt, motion::alpha(190, scrim));

                let cx = rect.center().x;
                let cy = rect.center().y - 5.0;
                let (pw, ph, gap) = (86.0, 30.0, 9.0);
                let (a0, r0) = sg.rise(0, 12.0);
                let (a1, r1) = sg.rise(1, 12.0);
                let copy_r = Rect::from_center_size(pos2(cx - pw / 2.0 - gap / 2.0, cy), vec2(pw, ph));
                let save_r = Rect::from_center_size(pos2(cx + pw / 2.0 + gap / 2.0, cy), vec2(pw, ph));

                if paint::pill_button(
                    ui, icons, pal, copy_r, cid.with("copy"), "copy", "Copy", true, a0 * flat,
                    vec2(0.0, r0),
                )
                .clicked()
                {
                    clicked = Some("Copied to clipboard");
                }
                if paint::pill_button(
                    ui, icons, pal, save_r, cid.with("save"), "device-floppy", "Save", false,
                    a1 * flat, vec2(0.0, r1),
                )
                .clicked()
                {
                    clicked = Some("Saved to Desktop");
                }

                // Four secondary actions in the corners (D12).
                let corners: [(&str, egui::Pos2, &'static str); 4] = [
                    ("pin", pos2(rect.left() + inset + d / 2.0, rect.top() + inset + d / 2.0), "Pinned"),
                    ("x", pos2(rect.right() - inset - d / 2.0, rect.top() + inset + d / 2.0), ""),
                    ("pencil", pos2(rect.left() + inset + d / 2.0, rect.bottom() - inset - d / 2.0), "Annotate"),
                    ("cloud-upload", pos2(rect.right() - inset - d / 2.0, rect.bottom() - inset - d / 2.0), "Uploading…"),
                ];
                for (n, (name, c, toast)) in corners.iter().enumerate() {
                    let (a, r) = sg.rise(2 + n, 8.0);
                    let br = Rect::from_center_size(*c, vec2(d, d));
                    let resp = paint::icon_button_id(
                        ui,
                        icons,
                        pal,
                        br,
                        cid.with(("corner", n)),
                        name,
                        15.0,
                        paint::BtnState::default(),
                        a * flat,
                        vec2(0.0, r),
                    );
                    if resp.clicked() {
                        if *name == "x" {
                            fling = Some((slot, vec2(anchor.sign() * 1100.0, -60.0)));
                        } else {
                            clicked = Some(toast);
                        }
                    }
                }

                // Caption sits on the same line as the bottom corner icons.
                let cap = motion::alpha(228, sg.at(1) * flat);
                if cap > 2 {
                    p.text(
                        pos2(rect.center().x, rect.bottom() - inset - d / 2.0),
                        Align2::CENTER_CENTER,
                        format!("{}  ·  {}", card.name, card.dims),
                        theme::ts_caption(),
                        Color32::from_white_alpha(cap),
                    );
                }
                if slot == 0 {
                    paint::count_badge(&p, rect.left_top() + vec2(-2.0, -2.0), count as u32, pal);
                }
            }

            p.rect_stroke(
                rect,
                cr(rt),
                Stroke::new(1.0, motion::fade(pal.thumb_border, card.alpha * flat)),
                StrokeKind::Inside,
            );

            active |= sg.animating();

            // ---- drag / fling ----------------------------------------------
            if resp.drag_started() {
                card.drag_target = card.drag.pos;
                *dragging = Some(card.id);
                vel.clear();
                vel.push(0.0, card.drag_target);
            }
            if resp.dragged() {
                card.drag_target += resp.drag_delta();
                vel.push(dt, card.drag_target);
                active = true;
            }
            if resp.drag_stopped() {
                *dragging = None;
                let s = anchor.sign();
                // Our own tracker, not egui's smoothed one — see VelocityTracker.
                let v = vel.velocity();
                // Only motion *toward the anchored edge* counts, in either test.
                // Dragging a card the other way is a drag-out, not a dismissal.
                let toward = v.x * s;
                let travelled = card.drag_target.x * s;

                if toward > motion::dismiss_vel() || travelled > motion::dismiss_dist() {
                    // Carry the real throw velocity, so a hard flick travels
                    // further and faster than a shove. A slow drag that merely
                    // passed the distance threshold gets a fixed launch so it
                    // still clears the frame instead of stalling mid-air.
                    fling = Some((
                        slot,
                        if toward > motion::dismiss_vel() {
                            vec2(v.x, v.y * 0.35)
                        } else {
                            vec2(s * 900.0, v.y * 0.25)
                        },
                    ));
                } else {
                    // Under threshold: spring home. This is the branch that
                    // makes the threshold *feel* like a threshold.
                    card.drag_target = Vec2::ZERO;
                    active = true;
                }
                vel.clear();
            }
            if over {
                ctx.set_cursor_icon(if held {
                    egui::CursorIcon::Grabbing
                } else {
                    egui::CursorIcon::Grab
                });
            }
        }

        if let Some((slot, v)) = fling {
            if slot < self.deck.len() {
                let s = self.anchor.sign();
                let mut c = self.deck.remove(slot);
                c.drag.vel = v;
                // Spin the way it is thrown.
                c.spin = (v.x * 0.0016).clamp(-2.2, 2.2) + s * 0.35;
                self.flying.push(c);
                self.stagger_deck();
                self.dragging = None;
                active = true;
            }
        }
        if let Some(t) = clicked {
            if !t.is_empty() {
                self.toast = Some((t.to_owned(), 0.0));
            }
        }

        // ---- cards in flight, above everything ------------------------------
        for c in &self.flying {
            let r = Self::geom(home, c.depth.pos).translate(c.drag.pos);
            paint::capture_face(&p, r, rt, c.angle, motion::alpha(255, c.alpha), c.variant, 1.0);
        }

        // ---- transient confirmation -----------------------------------------
        if let Some((text, age)) = &self.toast {
            let t = (age / TOAST_LIFE).clamp(0.0, 1.0);
            // Pop in over the first 12%, then rise and fade away. Reduce-motion
            // holds it flat and fully opaque until it expires.
            let (a, rise) = if motion::reduced() {
                (1.0, 0.0)
            } else {
                let a = if t < 0.12 { t / 0.12 } else { 1.0 - ((t - 0.12) / 0.88).powi(2) };
                (a, 10.0 * t)
            };
            // Below the list, not above it — the slot above `home` now holds a
            // real card instead of empty stack headroom.
            let c = pos2(home.center().x, home.bottom() + 24.0 + rise);
            let fg = motion::fade(pal.on_accent, a);
            let galley = p.layout_no_wrap(text.clone(), theme::ts_caption(), fg);
            let r = Rect::from_center_size(c, galley.size() + vec2(22.0, 12.0));
            p.rect_filled(r, cr(r.height() / 2.0), motion::fade(pal.accent, a));
            p.galley(r.center() - galley.size() / 2.0, galley, fg);
        }

        if self.deck.is_empty() && self.flying.is_empty() {
            p.text(
                home.center(),
                Align2::CENTER_CENTER,
                "Press  N  for a new capture",
                theme::ts_label(),
                pal.text_faint,
            );
        }

        active
    }
}
