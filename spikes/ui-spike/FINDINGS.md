# Scrozz UI spike — findings

**Spike question:** can Rust + egui/eframe be made genuinely beautiful — CleanShot-X-grade —
or is it too ugly to base Scrozz's shared custom-drawn UI on?

**Verdict up front:** **Qualified yes.** egui *can* reach CleanShot-grade polish for
2D chrome like Scrozz's — the beauty shots in `screenshots/` (the drag-first stack,
the drag-out, the swipe-to-dismiss, onboarding, menu, annotate, and the light variant)
are, in my honest judgement, at the bar. But "beautiful egui" is almost entirely
**egui-as-a-canvas**, not egui-as-a-widget-toolkit: you get there by hand-drawing with
`Painter`, not by styling built-in widgets. And the one thing the maintainer explicitly
asked for — **real macOS Liquid Glass behind crisp content in a single window** — did
*not* work out of the box and is the only thing that genuinely fought back. Details
below, blunt.

**Second question, answered in a later pass:** *can it be made to **feel** premium, not
just look it?* Motion was originally left unproven — this document said so — and became
the last risk that could have reopened the toolkit decision. It is now built and
hand-drivable. **Short answer: yes, for ~500 lines of motion-token layer, with one real
capability gap (epaint cannot rotate text or clipped images).**

The scope changed mid-build, and the change is itself a finding: the maintainer cut all
button and menu animation — *"the instant background changes are GOOD actually, make it
feel more responsive"* — and moved the whole budget onto the capture cards: sliding in
from the anchored screen edge, grabbable, and flung off with real pointer velocity.
**Motion belongs to objects that move through space; controls respond instantly.** That
turns out to suit egui, which gives you nothing for free but makes the physics
tractable. Full accounting with measurements in **§5**; run `cargo run`, press `N`, and
throw a card.

Look at the pixels first; this document is the argument, the screenshots are the evidence:

| Screenshot | What it shows |
|---|---|
| `quick_stack_dark.png` | **Primary.** Quick Access Overlay as a drag-first **stack** of captures — physical card-stack with depth falloff, a prominent grab tab, a count badge, and the secondary action bar. The headline result. |
| `quick_drag_dark.png` | **The hero interaction.** A capture dragged straight out of the corner stack into another app (Messages), with a drag-count badge, grab cursor, and motion trail. This is the "almost more intuitive than copy" gesture the maintainer called core. |
| `quick_swipe_dark.png` | **Swipe-to-dismiss.** The top capture flung downward off the stack, opaque, rotated, with faint ghost echoes tracing its path so the gesture reads in a still frame. |
| `quick_stack_light.png` | Light-mode stack — proves the token system has range. |
| `onboard_dark.png` | Onboarding step — big display title, muted subtitle, hero tile, primary pill in a separated footer. Exercises the type scale at large sizes, which nothing else does. |
| `menu_dark.png` | Menu-bar dropdown, ⇧⌘ shortcut hints, accent pill, hairline dividers. |
| `annotate_dark.png` | Annotation toolbar — selected / hover / default tool states. |
| `transparency_proof.png` | Card over the **real** desktop (desktop text bleeds through) — proves transparent/borderless/on-top windows are real. *(Shows an earlier single-card layout; the exact card is immaterial to what this one proves.)* |
| `liquidglass_over_content.png` | The failure: NSGlassEffectView frosts the **app** into ghosts. |
| `vibrancy_over_content.png` | The failure: NSVisualEffectView (HUD) fully occludes the app → flat grey. |

---

## 1. How hard was it to get egui to look good? What fought me?

Getting egui to *stop looking like egui* took real work, but it was **grind, not wall**.
Nothing about the look was impossible; a lot of it was un-ergonomic.

What fought me, roughly in order:

- **The default styling is the thing being disproven, and you must replace almost
  all of it.** Default egui reads as a debug tool: flat fills, hard 2 px radii, one
  weight of one font, no elevation, no optical spacing. I threw essentially all of it
  away and built a token layer (`theme.rs`: a colour ramp, a spacing scale, a radius
  scale, elevation/shadow, and a real type ramp with four Inter weights) and a custom
  `Style`/`Visuals`. This is exactly what Rerun's `re_ui` does, and after doing it I
  understand why they had to: **the polish does not live in egui, it lives in the
  token layer you bring.** That's a cost, but it's a one-time cost and it's the normal
  cost of a design system in *any* toolkit.
- **The built-in widgets can't hit the bar, so the token layer isn't enough** — see
  §3. Most of what looks good is drawn by hand.
- **No gradients.** epaint has no gradient primitive at all. Every soft transition in
  the shots — the scrim under the thumbnail caption, the wallpaper glow — is faked by
  stacking many translucent rects/circles. It looks right in a still; it is crude
  underneath and would need a real shader for moving gradients.
- **No rotation — for anything.** This is the cost the drag-first redesign surfaced.
  epaint can't rotate an image, a rounded rect, or (worst) a text galley. The swipe's
  flung card and the drag's lifted card are tilted, so *every* tilted element is built
  by hand: I generate a rounded-rectangle outline as a point list (`rounded_poly`),
  rotate the points about a pivot (`rotate_pts`), and fill the result with
  `Shape::convex_polygon`; even the "window chrome" stripes inside a tilted card are
  individually rotated rects. It works and looks right, but a rotated card is ~40 lines
  of geometry where a native toolkit is one transform. Rotated **text** I simply avoided
  — there is no honest way to do it in epaint without rendering text to a texture and
  rotating that, which I judged out of scope for a still. If Scrozz wants live tilt/spin
  in motion, budget for a texture-and-transform path.
- **`CornerRadius` is `u8` per-corner and shadows are a fixed struct.** Fine, but you
  feel egui nudging you toward "good enough" rather than "designed."
- **~~This environment ships a *patched* eframe/egui 0.36.1.~~ CORRECTION — this was
  wrong.** The `App` trait here really is `fn ui(&mut self, ui, frame)` (plus
  `all_styles_mut` / `set_style_of` and a `clear_color` override) — but that is
  **upstream**. Verified after this spike: `eframe 0.36.1` is the newest published
  release on crates.io, resolved from the registry with a checksum, and the vendored
  source at `~/.cargo/registry/src/*/eframe-0.36.1/src/epi.rs:182` carries exactly that
  signature. Nothing is patched or forked; upstream examples written against *this*
  version drop in fine.

  **The real lesson, which generalises:** the spike's memory of egui was older than the
  pinned crate, and the available explanation for the mismatch — "the environment must
  be nonstandard" — was wrong and cost real time. On a fast-moving 0.x dependency, read
  the vendored source under `~/.cargo/registry/src/` before writing against it, and
  never explain away a compile error by assuming a patched toolchain.

What did **not** fight me: layout (egui's immediate-mode layout is pleasant and fast
to iterate), iteration speed (sub-second incremental rebuilds), fonts, and SVG icons.

## 2. Did Liquid Glass / vibrancy work with egui? Compositing artifacts?

**This is the real negative result, and it's the one the maintainer cares about most.**

The window itself is genuinely transparent, borderless, and always-on-top — that all
works (`transparency_proof.png`: you can read desktop text bleeding through the window
around the card). So egui can absolutely produce free-floating HUD/overlay windows,
which is exactly the window *type* Scrozz needs. Good.

But **the actual native material composited in the wrong order.** Using
`window-vibrancy` on this eframe:

- `apply_liquid_glass` (NSGlassEffectView, the real macOS 26 Liquid Glass) **applied
  without crashing** — the API is reachable and does something — but it frosted the
  **egui content itself** into unreadable ghosts (`liquidglass_over_content.png`).
- `apply_vibrancy` with `HudWindow` was worse: it **fully occluded** the app, leaving
  a flat frosted grey rectangle (`vibrancy_over_content.png`).

Root cause: winit's layer-backed content view draws its GL/Metal contents, and
`window-vibrancy` inserts the `NSVisualEffectView`/`NSGlassEffectView` as a **subview**
of that same content view — i.e. *in front of* the egui drawing — so the material
blurs the app instead of blurring the desktop behind the window. To get "real OS glass
**behind** crisp egui content" you need the effect view to be a **sibling behind** the
GL layer, which means a custom `NSView` hierarchy (or an eframe/winit patch that
exposes one). That is **not a one-liner**; it's a focused chunk of macOS-native work.

**The workaround I shipped in the beauty shots** is to *draw the glass in egui* — a
dark translucent panel with hand-painted inner lighting, a hairline top highlight, a
soft double shadow, and a drawn wallpaper behind it. It looks great in a still (that's
`quick_stack_dark.png`), and for an opaque-ish card it's arguably indistinguishable. But it
is **not** real backdrop blur: it can't blur *whatever is actually behind the window*
on the user's screen. For a screenshot HUD that mostly sits over its own captured
image that's fine; for a translucent-over-the-live-desktop look it is not the same
thing, and pretending otherwise would be overselling.

So: **transparent/borderless/on-top windows — yes, proven. Real Liquid Glass behind
live content in one egui window — no, not without native `NSView` work.**

## 3. Custom `Painter` widgets: how much was needed vs built-ins?

**Nearly everything you see is hand-drawn.** This is the single biggest takeaway about
*how* you make egui beautiful, and it cuts both ways.

Drawn from scratch with `Painter` (`paint.rs`): the glass panels, all shadows, the
thumbnail's browser mock and its scrim gradient, the wallpaper, every icon button and
its hover/press/selected background, the menu rows and their accent pill, the
right-aligned shortcut chips, the annotation tool cells, the colour swatch, and the
stroke-width control. The whole drag-first overlay is bespoke painting too: the
physical **card stack** with per-depth scale/opacity falloff, the grab tab, the count
badge, the **drop-target app** (Messages mock) with its dashed accent border, the
**rotated** flung and lifted cards (§1), the drag-count badge, the arrow cursor, and
the ghost-echo / motion-trail cues that make a gesture read in a still. Built-in widgets
are used only for trivial text runs.

Interpretation: this is **expected and fine** — it's exactly what the spike was meant
to test, and it's how Rerun does it too. Immediate-mode + `Painter` is a *pleasant*
way to build custom controls; there's no fighting a widget's opinions because there is
no widget. **But** be honest about the implication: choosing egui for Scrozz is
choosing to **build the entire control library by hand.** There is no premium
component kit to lean on. For a small, bespoke, highly-controlled UI like a screenshot
tool that is a reasonable — even good — trade. For a sprawling settings/preferences
surface with dozens of standard controls, that hand-rolling cost recurs everywhere.

## 4. Text rendering quality — crisp enough? vs native.

**Crisp enough for a premium app, with one honest caveat.** At 2× (the native scale on
this display, and what the shots are rendered at) Inter renders clean and sharp;
weights are distinct, the ⇧⌘⌥⌃ glyphs come from the real font and are pixel-crisp, and
nothing looks fuzzy or fringed in `menu_dark.png` or the captions. I would ship this.

The caveat, stated plainly: egui uses a **grayscale** glyph atlas, not subpixel/LCD
AA, and its hinting is lighter than CoreText's. Side by side with a native `NSTextField`
at **1×**, native text is very slightly crisper and "grippier"; egui text is a touch
softer. At 2×/Retina the gap essentially vanishes. So: indistinguishable-to-excellent
on Retina, very slightly behind native on a low-DPI external display. For Scrozz's
target (modern Macs, mostly Retina) this is a non-issue; it's worth knowing for a
1080p Windows/Linux user.

## 5. Motion — what it actually cost

> **Status: now answered.** The previous pass of this spike shipped *static depictions*
> of movement — a flung card with ghost echoes, a lifted card with a motion trail — and
> said so plainly: *"I did not build animated transitions."* That gap has now been
> closed. The spike animates for real, is driven by hand, and is tunable live. This
> section replaces the earlier placeholder.

**Verdict up front: yes, egui can deliver premium micro-interactions, and the cost is
lower than I expected — but only because you build a motion layer once and then never
think about it again. Without that layer it would be miserable.** The gating risk was
never "can egui hit 60fps" (it trivially can); it was "does the immediate-mode model
fight you." It mostly doesn't. It fights you in exactly two places, both of which are
one-time costs, both documented below. I would not reopen the toolkit decision over
motion. There is one genuine capability gap — §5.6 — that constrains *what you can
animate*, and it is worth reading before signing off.

### 5.1 The scope correction — and why it made the build better

Halfway through, the maintainer corrected the brief, and the correction is the most
useful finding in this section:

> *"I think the button animations don't NEED to be anything, the instant background
> changes are GOOD actually. Make it feel more responsive."*
> *"I'm talking about swipe gestures and animating it in from the left when you take a
> new screenshot, then being able to slide it off to the left."*

The governing principle that came out of it:

> **Motion belongs to objects that move through space. Controls respond instantly.**

So the first pass — which dutifully eased every button hover, every pill press and
every menu row, exactly as the original brief asked — was **wrong**, and I removed it.
Easing a discrete control adds latency between intent and acknowledgement, and that
reads as *sluggish*. An instant state change reads as *responsive*. The physical
motion that makes an app feel alive belongs to things that have a position: cards.

This matters for the toolkit question in a way I did not anticipate. **The amount of
motion a premium app actually needs is much smaller than "animate everything", and it
is concentrated in exactly the place egui is weakest — free-form, physics-driven,
directly-manipulated objects.** A framework that makes button transitions easy and
gesture physics hard is optimised for the wrong half. egui is the opposite: it gives
you nothing for free and makes you write the physics, which turns out to be the half
that matters. That is a better trade than it first looks.

Concretely, de-animating the controls **deleted** code: `icon_button_id`, `pill_button`
and `menu_row` each lost their `motion::anim` calls, their press-shrink and their tint
lerps, and became three exclusive `if` branches. Instant is *less* code, not more.

### 5.2 What animates now

Run `cargo run`, press `N` a few times, then grab the front card. Everything here is
real motion, not a depiction:

- **A new capture enters from the anchored edge.** The card starts fully off-screen
  past the edge the overlay is docked to, travels in, **overshoots its home position and
  springs back**. Not a fade — a real slide with a settle. The direction is a
  parameter (`Anchor::Left` / `Right`, `A` to flip), because CleanShot's overlay is
  edge-anchored and the gesture language has to mirror when the anchor moves.
- **The deck reflows with a stagger.** Cards beneath don't snap to their new depths as
  one rigid block; each waits a configurable few tens of ms before starting to settle,
  so the stack ripples. This is a small detail that does a lot of work.
- **Drag is 1:1.** While you hold a card it sits *exactly* under the pointer — no
  spring, no lag. I tried spring-follow first (the original brief asked for "slight lag
  and inertia") and it feels like the card is fighting you. **Inertia belongs to the
  release, not the hold.** The card tilts into its direction of travel while held.
- **Fling carries real velocity.** Release past the throw-speed *or* drag-distance
  threshold and the card leaves at the speed of the last ~80 ms of your gesture,
  decelerating under friction, spinning the way it was thrown, and fading over the
  *distance* it travels rather than on a wall clock — so a hard flick stays solid until
  it is genuinely gone.
- **Below threshold it springs home**, which is what makes the threshold *feel* like a
  threshold rather than a number.
- **Only motion toward the anchored edge counts as a dismissal.** Dragging the other way
  reads as a drag-out and springs back.
- **The deck settles up** when a card leaves.
- **Hover reveal on a card** — scrim, then Copy/Save pills and the four corner icons
  rise in, staggered. Retained because the chrome is *revealed on an object*, not a
  control changing state.

Plus `R` to replay, `A` to flip the anchor, and `M` for the tuning overlay.

#### The single most important detail: don't use `pointer.velocity()`

egui exposes `PointerState::velocity()`, and using it makes the fling feel broken in a
way that is hard to diagnose. It is **smoothed for kinetic scrolling** and keeps
reporting motion for a while after the pointer has stopped. So the gesture
*"drag slowly, stop, let go"* — which a user performs meaning **"put it back"** —
registers as a throw, and the card flies off. It feels haunted.

The fix is 30 lines: keep a rolling ~80 ms window of `(dt, position)` samples and
difference the ends. A dead stop then reads as a dead stop. This is the difference
between a fling that feels physical and one that feels random, and it is a test case:
`velocity_tracker_reads_a_throw_and_ignores_a_dead_stop`.

**If you take one thing from this spike, take that.** It is not an egui limitation —
every framework ships a smoothed pointer velocity aimed at scroll views — but it is the
detail a hand-rolled gesture layer lives or dies on.

### 5.2b How much code it took

| Piece | Lines | What it is |
| --- | --- | --- |
| `src/motion.rs` | **~500** | The whole motion token layer, including velocity tracking and the live-tunable physics globals. This is the reusable part. |
| `src/stack.rs` | ~640 | The live animated surface — entry, drag, fling, reflow. Spike-specific; a real app writes this anyway. |
| `src/tuner.rs` | ~215 | The `M` tuning overlay. Dev tool, ships behind a flag or not at all. |
| `paint.rs` / `app.rs` / `surfaces.rs` | ~+310 | Key handling, repaint scheduling, and *removing* control animation. |
| `tests/motion.rs` + `tests/stack.rs` | ~980 | Headless verification (§5.7). |

**The number that matters is ~500, and about 120 of it is the part egui doesn't give
you at all**: two spring integrators, the velocity tracker, and the stagger helper.
Duration tokens, easing curves and the `Id`-keyed animation helper are thin wrappers —
egui already ships the genuinely hard part, which is per-`Id` state storage and
frame-delta accumulation keyed off a widget identity that doesn't exist as an object.

The honest read: **"egui has no animation system" costs about a day, once.** What it
does *not* give you is a scene graph, so anything with a position — the cards — you
integrate yourself. That is ~200 lines of `stack.rs` and it is the same physics you'd
write against any immediate-mode canvas.

### 5.3 Where immediate mode **helped**

This surprised me, and it's the strongest argument in egui's favour here.

- **There is no view tree to diff, so there is no "animating a thing that is being
  reconciled" problem.** In a retained-mode toolkit, animating an element that the
  framework might destroy and recreate is a classic source of pain. Here, animation
  state is a single float in a side table keyed by `Id`. Nothing can invalidate it.
- **Interruption is free.** Mouse out halfway through a hover reveal and it just
  reverses from wherever it is — no cancel/restart bookkeeping, no "animation in
  progress" state machine. This is the thing that makes reversal feel as good as the
  forward direction, and I got it for free.
- **Staggering is arithmetic, not orchestration.** A stagger is one master timeline
  plus an index offset. Twelve lines. No timeline objects, no keyframe graph.
- **Physics and timelines coexist trivially.** I deliberately used both — timelines
  (`motion::anim`) for hover/press, springs (`Spring1`/`Spring2`) for drag-follow, deck
  depth and fling coast — because velocity-based gestures want physics and discrete
  state changes want curves. In immediate mode both are just "compute a number this
  frame." No impedance mismatch.

### 5.4 Where immediate mode **hurt** — the two real costs

**(1) Nothing repaints unless you ask, and the failure is silent.** This is the #1
gotcha and it deserves the flag. egui repaints on input only; an animation with no
`request_repaint()` doesn't error, it just… crawls. `elapsed` is clamped to
`stable_dt`, so a missed repaint doesn't jump-cut, it *inches forward on the next
mouse move* — which reads as "the animation is broken" rather than "the scheduler is
wrong," and is genuinely hard to diagnose by eye.

Worse, there are **two scheduling models to reconcile**. egui's `animate_*_with_time`
helpers self-schedule (they call `request_repaint` internally and stop when they land).
My springs do not — they're my own integrators, so the app has to schedule them. I
resolved this by having `Stack::show` return a single `active: bool` that ORs together
every in-flight spring, entry, fling and toast, and `app.rs` does `if busy {
ctx.request_repaint() }`. That is the whole mechanism, and it is about six lines, but
you must design for it from the start.

Getting this right is itself part of what the spike proves, because "just repaint
forever" would invalidate the native-performance argument that justified egui:

> **Measured idle cost: 0.0% CPU, ~110 MB RSS** (release build, window open, nothing
> moving, sampled over 10s). It animates smoothly and then genuinely goes to sleep.

The one exception: the `M` tuner overlay requests repaint continuously while open,
because it draws a live sweeping curve preview. That's a dev overlay and it's
deliberate — but it is not free and shouldn't ship enabled.

I also found a real bug this way, which is a good advert for the discipline: my toast
confirmation held `active = true` for its entire 1.25s dwell, meaning ~75 identical
repainted frames *even with reduce-motion on*. The fix is
`ctx.request_repaint_after(remaining)` — sleep until it expires rather than busy-wait.
Distinguishing "I am animating" from "I am waiting" is a real discipline egui imposes,
and it's the correct discipline.

**(2) Global motion state breaks parallel tests.** The duration multiplier and
reduce-motion switch are process-wide atomics — that's what makes them settable from
one tuner slider and readable from every call site without threading a context object
through every function. But `cargo test` runs tests in parallel threads of one
process, so tests that set them stomp each other. I had to serialise them behind a
`Mutex` guard. That is a genuine, if minor, cost of the global-token design, and it's
worth knowing before copying the pattern into production.

### 5.5 What feels good

- **The entry slide.** A card arriving from off-screen and overshooting slightly before
  settling is the interaction that sells the whole thing. It costs one spring.
- **Instant controls next to physical cards.** The contrast is the point: the UI
  acknowledges you immediately, and the *objects* move. This reads more premium than
  animating both, and it was the maintainer's call, not mine.
- **1:1 drag.** Removing the spring-follow I originally built made it dramatically
  better. Direct manipulation should be direct.
- **The velocity-based fling.** A flick genuinely throws the card and a slow drag
  genuinely doesn't — and, critically, a drag that *stops* before release doesn't
  either.
- **Interruption is free.** Grabbing a card mid-entry, or flicking one while the deck is
  still settling, just works, because there is no animation object to cancel — see §5.3.
- **Reduce-motion (D13) is one line at the choke point.** Every duration goes through
  `motion::dur()`, which returns `0.0` when reduce is set; springs snap to target.
  That's the whole implementation, and it's verified by test rather than by eye.
- **Idle really is idle.** Measured on the release build: after the entry animation
  lands, CPU time stops advancing entirely — `0:00.91 → 0:00.91` over 12 seconds, zero
  repaints. The "native performance" claim survives adding motion.

### 5.6 What still feels wrong, or was not achievable

Being blunt, because this is the part that's actually useful:

- **epaint cannot rotate text. This is the real constraint, and it shapes the design.**
  Confirmed again this pass: no transform for images, rounded rects, or text galleys. I
  rotate cards by generating a rounded-rect outline as a point list, rotating the points
  about a pivot, and filling with `convex_polygon`. That works for *shapes*. It cannot
  work for text or images. Two concrete consequences:
  1. **The live card can't show the rich photo art the static screenshots use** — those
     compose clipped images, and clipping is axis-aligned only, so a rotated card can't
     be clipped. The animated card uses a simpler drawn face. The static screenshots are
     therefore slightly prettier than the live build, which is an artefact of this
     limitation, not of taste.
  2. **Chrome must fade out as the card tilts**, because the labels can't rotate with
     it. I made this deliberate — chrome fades as `|angle|` grows — and it reads as
     intentional. But it *is* a workaround, and if a future design needs a rotated label
     the answer is render-to-texture, which is a real project.
- **No gradient primitive**, so gradients are stacks of ~16 translucent rects. Animating
  those means animating the whole stack's opacity, not recomposing per frame. Fine here;
  a constraint to design around.
- **Spring constants and thresholds are still guesses.** I cannot see the screen, so I
  tuned by reasoning, not by feel. This is exactly why the `M` overlay exists, and why
  its sliders are now stiffness / damping / friction / threshold rather than durations:
  the numbers are live-adjustable so the maintainer can dial them in a minute rather
  than describe them to an agent over three round-trips. **Expect to move them.**
- **Drag-out (dragging a card into another app) is not really distinguished from
  swipe-to-dismiss.** The spike uses direction — motion toward the anchored edge is a
  dismissal, away from it springs back — which is a reasonable heuristic but not the
  real thing. A production build needs a platform drag-and-drop session, which is an OS
  integration question, not an egui one. The maintainer said to prioritise
  swipe-to-dismiss if it was ambiguous, and I did.
- **egui's own `pointer.velocity()` is unusable for flings.** Covered in §5.2 — it is
  smoothed for scrolling and reports phantom motion after a stop. Not a blocker, but a
  trap that costs you an afternoon if you don't know.
- **Not attempted:** motion blur, animated backdrop blur, cross-surface shared-element
  transitions. All three are plausible-to-hard and none were needed to answer the
  question.

### 5.7 How motion was verified without eyes

An agent cannot judge whether something looks smooth, so motion is verified
**mechanically over a simulated clock** — 19 headless tests across three suites:

- `tests/motion.rs` (10) — drives the primitives frame by frame and asserts the value
  actually moves (≥6 distinct intermediate frames), is monotonic, lands *exactly* on
  target, **schedules repaints while in flight and stops when done**, that reduce-motion
  snaps on frame one, that the multiplier scales elapsed time, that stagger cascades in
  index order, that every easing curve is anchored at 0/1 and overshoots only where
  designed, and that springs converge and lose energy while coasting. Plus the two that
  guard the corrected scope: that the **velocity tracker reads a hard flick as a large
  velocity but a drag-that-stopped-before-release as zero**, and that every live-tunable
  physics global round-trips and resets.
- `tests/stack.rs` (8) — drives the *real* surface through `egui_kittest`, fingerprinting
  every animated value per frame. Beyond "entry animates then goes idle", "dismiss
  travels then stops", "reduce-motion produces zero animated frames", "3× multiplier
  measurably lengthens the entry" and "replay restarts cleanly without leaking state",
  three pin down the gesture language specifically:
  - the card **starts >300 px outside the frame**, travels inward over several frames,
    and **overshoots past home before settling** — if the spring ever silently degrades
    into a plain ease-out, that assertion fails;
  - flipping the anchor **mirrors** the entry, proving direction is a parameter and not
    a hardcoded left;
  - a dismissal **travels toward the anchored edge** and actually leaves the deck,
    rather than fading in place.

These catch the two failure modes invisible to inspection: *the value never moves*, and
*you forgot `request_repaint` so it silently froze*. Both bit me during development and
both were caught by test rather than by luck.

Two findings worth recording for whoever writes the real tests:

- **epaint panics on drop if `FullOutput.textures_delta` is never consumed** — any
  headless driver must `clear()` it.
- **`animate_bool` snaps to target on the first frame it sees an `Id`.** Good product
  behaviour (a widget that appears already-hovered shows chrome immediately) but tests
  must "prime" the `Id` in its resting state for one frame first, or every animation
  test trivially passes with zero frames of motion.
- **A dwell timer that holds `active = true` for its whole life is a bug, not a
  feature.** The toast pinned the repaint loop for 1.25 s doing nothing. The fix is
  `ctx.request_repaint_after(remaining)` — sleep until the deadline instead of
  busy-waiting. Found by the reduce-motion test, which noticed 25 "animated" frames in a
  build that was supposed to have none.
- **Measure idle CPU with exactly one instance running.** I chased a phantom 11 % idle
  cost for a while before realising I had left a second copy of the binary running from
  an earlier launch. With one instance, idle is a hard zero.

The existing snapshot test also still passes **byte-identical**, which is a useful
proof in itself: the animated widget refactor changed no resting pixels. (It caught one
real regression on the way — `Color32::from_rgb` forces alpha to 255, so my colour lerp
was silently making muted icon tints opaque. 1132 drifted pixels, found by CI rather
than by eye.)

### 5.8 Bottom line for the toolkit decision

**Motion is not a reason to reject egui.** The ~500-line token layer — of which only
~120 lines are things egui gives you nothing for — is the entire tax for "egui has no
animation system." Immediate mode turns out to be an *asset* for interruptible,
gesture-driven motion: free interruption and no reconciliation problem are exactly what
you want for a drag-first UI like Scrozz's. The repaint-scheduling discipline is real
but small, and getting it right yields a measured **hard-zero idle CPU**.

Two caveats, in order of how much they should worry you.

**1. Rotation, not motion, is the real capability gap.** egui will animate anything you
can draw, but you cannot draw rotated text or rotated clipped images. Any interaction
whose design depends on tilting rich content needs a render-to-texture path or a
different design. For the capture card, designing around it was straightforward and
arguably better — chrome fades as the tilt grows, which reads as intentional. For
something like a rotating annotation handle with a live label, it would be a genuine
fight.

**2. There is no scene graph, so every moving object is yours to integrate.** This is
the cost people underestimate. egui hands you per-`Id` state and a frame delta; it does
not hand you "this thing has a position and a velocity." Springs, friction, velocity
tracking, stagger scheduling and hit-testing against animated geometry are all code you
write. It is ~200 lines for the card stack here and it was not hard — but it scales with
the number of *distinct physical behaviours* in the product, not with the number of
screens.

Against that: the scope correction (§5.1) showed that a premium app needs far less
motion than "animate everything," and the motion it does need is concentrated in
exactly this physics-driven category. So you pay the cost egui charges, in the one area
that actually matters, and you get instant controls for free because instant is the
correct answer for controls. That is a better fit than it first appears.

**Recommend proceeding with egui.** Promote the motion layer from this spike into a real
module, and re-tune the spring constants by hand on day one — they are reasoned, not
felt, and the `M` overlay exists so that takes minutes.

## 6. Did egui_kittest headless snapshot testing work?

**Yes — and this is a genuinely strong positive for the CI/agent story.**
`tests/snapshot.rs` renders the Quick Access overlay (the drag-first stack) through
**offscreen wgpu (Metal here;
Vulkan/llvmpipe on Linux CI) with no display server** and diffs it against a committed
PNG baseline (`tests/snapshots/quick_access.png`). `cargo test --test snapshot` passes
deterministically; `UPDATE_SNAPSHOTS=1` regenerates the baseline. So an agent *can*
verify Scrozz's UI headlessly on all three platforms — the exact thing the team needs
to review UI changes without a human eyeballing every diff.

Two non-obvious gotchas, documented so the next person doesn't lose an hour:

- **Font timing.** `set_fonts` applies on the *next* begin-pass, so drawing custom-font
  text on the same pass panics (`FontFamily::Name("medium") is not bound`). The test
  installs fonts on the first pass, requests a repaint, returns without drawing, then
  draws on later passes. The real binary avoids this because fonts install in
  `CreationContext` before frame 1.
- **8 px phantom margin.** kittest wraps the closure in a central panel with an 8 px
  outer margin, so the usable rect is inset 8 px/side; the harness size is padded by
  16 px/axis to compensate. Without this the headless layout is subtly compressed
  versus the windowed shot.

Neither is a blocker; both are one-time. The capability is real.

## 7. Bottom line: can egui reach CleanShot-grade polish? What would it cost?

**Yes for the visual bar — the shots prove it — but go in clear-eyed about *what*
you're buying.** egui is a beautiful **canvas**, not a beautiful **widget kit**. You
reach CleanShot-grade by (a) building a real design-token layer and custom `Style`, and
(b) hand-drawing essentially every control with `Painter`. That's the same path Rerun
took, it works, and for a small bespoke tool like Scrozz it's a *reasonable* amount of
work — call it the cost of owning a design system plus a hand-built control library.

Worth calling out because it was the maintainer's own steer: the **drag-first stack**
model — captures physically stacked in the corner, swipe-to-dismiss, and drag-straight-
into-another-app as the hero action — was entirely depictable at this bar, and is
arguably where egui-as-canvas *shines*: a bespoke physical metaphor like a card stack
with depth falloff and a grabbable object is exactly the kind of thing a canvas does
better than a widget kit, because there's no stock component fighting you. The cost it
did add is rotation (§1): tilted cards are hand-built from rotated polygons, and rotated
text is off the table without a texture path.

The costs to price in before committing the whole app:

1. **You will hand-build the entire control library.** No premium components to lean
   on. Fine for Scrozz's size; recurring for large surfaces.
2. **You will hand-build the motion layer, and every moving object's physics** — but
   this is priced now, not speculative: **~500 lines** for the token layer, of which
   only ~120 are things egui gives you nothing for, plus ~200 lines for the card
   stack's springs and gestures (§5). There is no scene graph, so anything with a
   position is yours to integrate; that cost scales with the number of distinct
   *physical behaviours*, not screens. Immediate mode genuinely *helps* here —
   interruption and reversal are free. The discipline it imposes is repaint scheduling:
   animate, then genuinely idle (**measured: CPU time stops advancing entirely at
   rest**). Note that controls should be instant anyway, so the motion surface area is
   smaller than it first looks.
3. **Real macOS Liquid Glass behind live content needs native `NSView` work** — an
   eframe/winit patch or a custom view hierarchy. This is the one item that is *not*
   cheap and *not* a library call. If "true OS glass over the live desktop" is a
   must-have identity feature, budget a dedicated native spike for it; if a drawn
   glass card over the captured image is acceptable (it looks great — `quick_stack_dark.png`),
   you already have it.
4. **~~This patched eframe fork diverges from upstream.~~ FALSE — see §1.** eframe
   `0.36.1` is stock, current and unmodified; the divergence was a misdiagnosis. No
   action needed. (The one *genuine* dependency risk is `window-vibrancy`, pinned to an
   unreleased git revision for `apply_liquid_glass`.)
5. **No gradient primitive, grayscale text AA, and no rotation for text or clipped
   images.** The first two are papercuts on Retina. The third is a genuine design
   constraint once things move (§5.6): you can tilt shapes, not labels.

If the maintainer's fear was "egui is inherently ugly," this spike **disproves it** —
the pixels are premium. If the follow-up fear was "but it'll feel dead," §5 disproves
that too. The honest amendment is: egui isn't ugly, it's *bare*. It ships
you a fast, cross-platform, headlessly-testable canvas and hands you the entire visual
design — and the entire motion design — as *your* job. Scrozz is small and opinionated
enough that that trade is a **yes** — with the Liquid-Glass-behind-content caveat
flagged in bright red.
