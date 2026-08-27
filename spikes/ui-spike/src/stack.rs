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

pub const CARD_W: f32 = 300.0;
pub const CARD_H: f32 = 188.0;
const BX: f32 = 11.0; // deck horizontal peek
const BY: f32 = 13.0; // deck vertical peek
/// How long the "Copied" / "Dismissed" confirmation stays up, in seconds.
const TOAST_LIFE: f32 = 1.25;

/// Release speed (px/s) above which a drag becomes a throw.
const FLING_SPEED: f32 = 520.0;
/// Displacement (px) above which a slow drag still dismisses on release.
const FLING_DIST: f32 = 96.0;

pub struct Card {
    pub id: u64,
    variant: usize,
    name: String,
    dims: &'static str,
    /// Animated position in the deck. Target is the card's index, so inserting
    /// or removing a card makes every other card spring to its new slot.
    depth: Spring1,
    /// Offset from the card's home position. Chases `drag_target` while held,
    /// then coasts ballistically once flung.
    drag: Spring2,
    drag_target: Vec2,
    angle: f32,
    spin: f32,
    alpha: f32,
    /// 0→1 entry ramp, used for the fade and the unwinding entry tilt.
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
            angle: 0.0,
            spin: 0.0,
            alpha: 1.0,
            entry: 1.0,
        }
    }

    /// Seed the "just captured" state: in front of the deck, high and to the
    /// left, falling in. The depth spring then pulls it back to slot 0.
    fn seed_entry(&mut self) {
        self.depth = Spring1 { pos: -1.7, vel: 0.0 };
        self.drag = Spring2 { pos: vec2(-22.0, -96.0), vel: vec2(40.0, 210.0) };
        self.drag_target = Vec2::ZERO;
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
        };
        for i in 0..4 {
            let (name, dims) = label(s.clock);
            s.clock += 1;
            let id = s.next_id;
            s.next_id += 1;
            s.deck.push(Card::new(id, i, name, dims, i as f32));
        }
        s
    }

    /// `N` — a new capture lands on top and the deck settles back.
    pub fn spawn(&mut self) {
        let (name, dims) = label(self.clock);
        self.clock += 1;
        let id = self.next_id;
        self.next_id += 1;
        let mut c = Card::new(id, (id as usize + 1) % 4, name, dims, 0.0);
        c.seed_entry();
        self.deck.insert(0, c);
        // Keeping the deck shallow means the springs behind stay legible.
        self.deck.truncate(6);
        self.toast = Some(("Captured".into(), 0.0));
    }

    /// `Backspace` — throw the top card without a gesture, so the dismiss
    /// animation is reachable from the keyboard.
    pub fn dismiss_top(&mut self) {
        if self.deck.is_empty() {
            return;
        }
        let mut c = self.deck.remove(0);
        c.drag.vel = vec2(760.0, -180.0);
        c.spin = 1.5;
        self.flying.push(c);
    }

    /// `R` / tuner Replay — re-run the entry animation for the whole deck so the
    /// maintainer can watch the same motion repeatedly without restarting.
    pub fn replay(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.flying.clear();
        for (i, c) in self.deck.iter_mut().enumerate() {
            c.depth = Spring1 { pos: -1.7 - i as f32 * 0.35, vel: 0.0 };
            c.drag = Spring2 {
                pos: vec2(-22.0 - i as f32 * 6.0, -96.0 - i as f32 * 10.0),
                vel: vec2(40.0, 210.0),
            };
            c.drag_target = Vec2::ZERO;
            c.entry = 0.0;
            c.alpha = 1.0;
            c.angle = 0.0;
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

    /// Geometry for a card at fractional deck depth `d`. At `d == 0` this is the
    /// hero rect; at 1/2/3 it matches `surfaces::deck_behind_n` exactly; at
    /// negative `d` the card is bigger and lower — which is what the entry
    /// animation springs *from*.
    fn geom(home: Rect, d: f32) -> Rect {
        Rect::from_min_size(home.min + vec2(-BX * d, -BY * d), home.size()).shrink(3.0 * d)
    }

    /// Deck tint for a fractional depth, interpolating the discrete steps the
    /// static scenes use so a card sliding from slot 2 to slot 1 doesn't jump.
    fn deck_style(d: f32) -> (Color32, Color32, u8) {
        const STOPS: [([u8; 3], [u8; 3], f32); 3] = [
            ([0xD9, 0xDD, 0xEC], [0xF3, 0xF4, 0xFA], 24.0),
            ([0xEC, 0xDA, 0xEC], [0xF6, 0xF2, 0xFA], 58.0),
            ([0xD7, 0xEC, 0xE4], [0xF0, 0xF6, 0xF4], 96.0),
        ];
        let x = (d - 1.0).clamp(0.0, 2.0);
        let i = (x.floor() as usize).min(1);
        let f = x - i as f32;
        let (h0, b0, d0) = STOPS[i];
        let (h1, b1, d1) = STOPS[i + 1];
        let mix = |a: [u8; 3], b: [u8; 3]| {
            Color32::from_rgb(
                motion::lerp(a[0] as f32, b[0] as f32, f) as u8,
                motion::lerp(a[1] as f32, b[1] as f32, f) as u8,
                motion::lerp(a[2] as f32, b[2] as f32, f) as u8,
            )
        };
        (mix(h0, h1), mix(b0, b1), motion::lerp(d0, d1, f) as u8)
    }

    /// Draw + drive one frame. Returns `true` while any spring is still moving,
    /// which the app turns into a `request_repaint` — the one thing that has to
    /// be right for hand-rolled physics to animate at all in immediate mode.
    pub fn show(&mut self, ui: &mut Ui, icons: &IconStore, pal: &Palette, scene: Rect) -> bool {
        let ctx = ui.ctx().clone();
        let dt = motion::dt_of(&ctx);
        let rt = theme::R_THUMB;
        let mut active = false;

        let home = Rect::from_min_size(
            pos2(scene.right() - 30.0 - CARD_W, scene.bottom() - 34.0 - CARD_H),
            vec2(CARD_W, CARD_H),
        );

        // ---- physics -------------------------------------------------------
        for (i, c) in self.deck.iter_mut().enumerate() {
            active |= c.depth.step(i as f32, dt, motion::K_SOFT, motion::C_SOFT);
            let target = c.drag_target;
            active |= c.drag.step(target, dt, motion::K_DRAG, motion::C_DRAG);
            active |= c.advance_entry(dt);
            // Tilt tracks lateral offset *and* velocity, so the card leans into
            // a throw before it has travelled far. Entry adds a tilt that
            // unwinds as the card lands.
            c.angle = (c.drag.pos.x * 0.0016 + c.drag.vel.x * 0.00010).clamp(-0.26, 0.26)
                - 0.09 * (1.0 - c.entry);
        }
        for c in self.flying.iter_mut() {
            c.drag.coast(dt, 1.5, vec2(0.0, 1500.0));
            c.angle += c.spin * dt;
            let fade = if motion::reduced() { 1.0 } else { dt * 1.7 };
            c.alpha = (c.alpha - fade).max(0.0);
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

        // ---- deck behind the front card ------------------------------------
        let p = ui.painter().clone();
        if let Some(back) = self.deck.last() {
            let r = Self::geom(home, back.depth.pos.max(0.0));
            paint::soft_shadow(&p, r.shrink(4.0), rt, pal, 0.7);
        }
        for c in self.deck.iter().skip(1).rev() {
            let d = c.depth.pos;
            let r = Self::geom(home, d).translate(c.drag.pos);
            let (h, b, dim) = Self::deck_style(d);
            paint::mini_capture_card(&p, r, rt, h, b, dim);
        }

        // ---- the front card -------------------------------------------------
        let epoch = self.epoch;
        let mut fling: Option<Vec2> = None;
        let mut clicked: Option<&'static str> = None;
        let count = self.deck.len();

        if let Some(card) = self.deck.first_mut() {
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

            paint::capture_face(
                &p,
                rect,
                rt,
                card.angle,
                motion::alpha(255, card.alpha * card.entry.max(0.15)),
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
            let flat = (1.0 - (card.angle.abs() / 0.10)).clamp(0.0, 1.0);
            let scrim = scrim * flat;
            let inset = 9.0;
            let d = 27.0;

            if scrim > 0.004 {
                p.rect_filled(rect, cr(rt), Color32::from_black_alpha(motion::alpha(96, scrim)));
                paint::bottom_scrim(&p, rect, 52.0, rt, motion::alpha(190, scrim));

                let cx = rect.center().x;
                let cy = rect.center().y - 6.0;
                let (pw, ph, gap) = (94.0, 32.0, 10.0);
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
                for (i, (name, c, toast)) in corners.iter().enumerate() {
                    let (a, r) = sg.rise(2 + i, 8.0);
                    let br = Rect::from_center_size(*c, vec2(d, d));
                    let resp = paint::icon_button_id(
                        ui,
                        icons,
                        pal,
                        br,
                        cid.with(("corner", i)),
                        name,
                        15.0,
                        paint::BtnState::default(),
                        a * flat,
                        vec2(0.0, r),
                    );
                    if resp.clicked() {
                        if *name == "x" {
                            fling = Some(vec2(700.0, -200.0));
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
                paint::count_badge(&p, rect.left_top() + vec2(-2.0, -2.0), count as u32, pal);
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
            }
            if resp.dragged() {
                card.drag_target += resp.drag_delta();
                active = true;
            }
            if resp.drag_stopped() {
                let v = ctx.input(|i| i.pointer.velocity());
                if v.length() > FLING_SPEED || card.drag_target.length() > FLING_DIST {
                    // Give a slow-but-far drag enough push to clear the frame.
                    let raw = if v.length() > 60.0 { v } else { card.drag_target };
                    let dir = if raw.length() > 1.0 { raw.normalized() } else { vec2(1.0, 0.0) };
                    fling = Some(if v.length() > FLING_SPEED { v } else { dir * 720.0 });
                } else {
                    card.drag_target = Vec2::ZERO;
                }
            }
            if over {
                ctx.set_cursor_icon(if held {
                    egui::CursorIcon::Grabbing
                } else {
                    egui::CursorIcon::Grab
                });
            }
        }

        if let Some(v) = fling {
            let mut c = self.deck.remove(0);
            c.drag.vel = v;
            c.spin = (v.x * 0.0022).clamp(-2.6, 2.6);
            self.flying.push(c);
            active = true;
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
            let c = pos2(home.center().x, home.top() - 26.0 - rise);
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
