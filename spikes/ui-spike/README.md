# Scrozz UI spike (THROWAWAY)

A disposable visual spike that answers **one** question:

> Can **Rust + egui/eframe** be made genuinely beautiful — CleanShot-X-grade — for
> Scrozz's single shared custom-drawn UI, or is egui too ugly to build a premium
> product on?

**It is not a prototype of Scrozz.** Nothing here captures a screenshot, saves a
file, or talks to the OS beyond opening windows. Every surface is faked pixels.
This whole directory is expected to be **deleted or rewritten** once the toolkit
decision is made. It is deliberately *not* a Cargo workspace member and touches
nothing else in the repo.

The verdict lives in **[`FINDINGS.md`](./FINDINGS.md)**. The pixels the decision
rests on live in **[`screenshots/`](./screenshots/)**. Motion — the second
question, *"can it be made to **feel** premium, not just look it?"* — is answered
by running it and driving it by hand; see **[Run it](#run-it)** below and
FINDINGS §5.

## What it draws

Four surfaces, built with a bespoke dark-glass design-token theme (no default
egui styling), real vendored fonts (Inter) and real vendored SVG icons (Tabler,
rasterised through `resvg` — no emoji/glyph substitutes):

1. **Recent Captures Overlay** (primary) — the post-capture corner UI, modelled as a
   **drag-first stack**: multiple captures physically stacked with depth falloff, a
   prominent grab tab, a count badge, and a secondary action bar (copy · save ·
   annotate · pin · cloud-upload · close). Rendered in three states — **stack**
   (at rest), **swipe** (top card flung away to dismiss), and **drag** (a capture
   dragged straight out into another app, the hero interaction). Dark, plus a
   light-mode stack variant.
2. **Menu-bar dropdown** — capture-mode list with right-aligned ⇧⌘ shortcut
   hints and section dividers.
3. **Annotation toolbar** — crop · arrow · rectangle · ellipse · line · text ·
   highlighter · blur · pixelate · counter · pencil, plus a colour swatch,
   stroke-width control, and undo/redo, with selected / hover / default states.
4. **Onboarding step** — big display title, muted subtitle, hero tile, and a
   primary pill in a separated footer. A test of the type scale at large sizes.

## Run it

```sh
cd spikes/ui-spike

# Rust is user-local via rustup; each shell must source the env first.
source "$HOME/.cargo/env"

cargo run
```

That opens the interactive window on the **live Recent Captures Overlay** — a real,
animated, hand-drivable surface. Everything below moves; grab a card and throw
it.

### Keys

| Key | What it does |
| --- | --- |
| `N` | **Spawn a new capture** — the card slides in from off-screen past the anchored edge and settles on a spring; the deck beneath reflows with a stagger |
| `Backspace` / `Delete` | **Dismiss the top card** — throws it back out toward the anchored edge; the deck settles up |
| `A` | **Flip the anchor** left ↔ right. Cards enter from, and exit toward, whichever edge the overlay is docked to |
| `R` | **Replay** the entry animation (watch it as many times as you like) |
| `M` | **Motion tuner** overlay — see below |
| `1` `2` `3` `4` | Switch surface — quick access · menu · annotate · onboard |
| `V` | Cycle the Quick surface: **live** → stack → swipe → drag (the last three are the original *static* depictions, kept for comparison) |
| `L` | Light / dark |
| `G` | Drawn backdrop on / off |
| `Q` / `Esc` | Quit |

The same legend is drawn along the bottom of the window, so none of this
requires reading the source.

### Gestures — this is the spike

Everything worth judging is on the **capture cards**. Press `N` a few times to
build a deck, then grab the front card.

| Gesture | What should happen |
| --- | --- |
| **New capture (`N`)** | The card travels in from *off-screen past the anchored edge* — a real slide, not a fade — overshoots its home position slightly and springs back. Cards already in the deck settle to their new depths with a short per-card stagger, so the stack ripples rather than snapping as one rigid block. |
| **Grab and drag a card** | It tracks the pointer **1:1** — no lag, no spring fighting you while you hold it. It tilts into the direction of travel, and lifts slightly off the deck. Inertia belongs to the *release*, not the hold. |
| **Flick it toward the anchored edge** | Velocity-based throw. The card leaves carrying the speed of the last ~80 ms of your drag, decelerates under friction, spins the way it was thrown, and fades out over the distance it travels — so a hard flick stays solid until it is genuinely gone. The deck settles up behind it. |
| **Drag it a little and let go** | Under the threshold it springs home. The threshold is *either* throw speed *or* drag distance, both live-tunable in the `M` overlay. |
| **Drag it slowly, stop dead, then release** | It must **not** throw. This is the case egui's own smoothed `pointer.velocity()` gets wrong, and why the spike tracks velocity itself. |
| **Drag it *away* from the anchored edge** | Not a dismissal — that reads as a drag-out, and the card springs back. |
| **Hover a card** | The chrome reveals: scrim fades up, then Copy/Save pills and the four corner icons rise in, staggered a few ms apart. Mouse out reverses it. |

**Controls are deliberately *not* animated.** Buttons, pills and menu rows change
state instantly — no fade, no scale, no transition. That is a decision, not an
omission: easing a discrete control makes an app feel sluggish, and instant
feedback reads as *responsive*. Motion is reserved for objects that move through
space. If you want to see the difference, the timeline tokens are still wired up
under the tuner's collapsed section.

### Motion tuner (`M`)

A live overlay for dialling in the feel without a rebuild. Every slider writes
straight into `motion`'s globals, so animations already in flight pick the change
up on the next frame.

**Card gesture physics** — the numbers that actually decide the feel:

- **settle k / damp** — the spring that carries a card to its home position.
  Higher `k` snaps harder; lower damping adds overshoot and bounce.
- **deck k / damp** — the softer spring the cards *beneath* use to change depth.
- **fling drag** — friction on a thrown card. Low values let it sail.
- **stagger** — seconds between each deck card starting to settle. `0` makes the
  stack rigid; the default gives it a ripple.

**Dismiss threshold:**

- **throw speed** (px/s) and **drag distance** (px) — exceed *either* and the
  card leaves; below both it springs back.

Plus **Reset physics**, an **Anchor: left/right** toggle, a **Reduce motion**
checkbox (the D13 accessibility path — every duration collapses to zero, nothing
animates, everything still works), and **Replay / New / Dismiss** buttons so the
overlay is self-sufficient.

Timeline tokens (duration multiplier, easing override and the sweeping curve
preview) are still there under a collapsed header. They only reach the hover
reveal now, and the preview is the one thing in the app that repaints
unconditionally — hence keeping it behind a fold.

### Reproduce a screenshot

Each PNG in `screenshots/` is a capture of the **real running window** (so the
transparency/compositing is genuine, not mocked). The app can render one surface
to a borderless on-top window and self-capture:

```sh
cargo run -- --surface quick --variant stack --theme dark  --backdrop on --shot screenshots/quick_stack_dark.png
cargo run -- --surface quick --variant drag  --theme dark  --backdrop on --shot screenshots/quick_drag_dark.png
cargo run -- --surface quick --variant swipe --theme dark  --backdrop on --shot screenshots/quick_swipe_dark.png
cargo run -- --surface quick --variant stack --theme light --backdrop on --shot screenshots/quick_stack_light.png
cargo run -- --surface onboard              --theme dark  --backdrop on --shot screenshots/onboard_dark.png
cargo run -- --surface menu                 --theme dark  --backdrop on --shot screenshots/menu_dark.png
cargo run -- --surface annotate             --theme dark  --backdrop on --shot screenshots/annotate_dark.png
```

Flags: `--surface quick|menu|annotate|onboard` ·
`--variant live|stack|swipe|drag` (Quick surface only; `--shot` renders the
static variants — `live` is the interactive one) · `--theme dark|light` ·
`--backdrop on|off` (the in-egui drawn wallpaper) ·
`--material none|vibrancy|glass` (native macOS material — see FINDINGS) ·
`--shot <path>` (render one frame to PNG and exit).

### Headless motion tests (the "did it actually move?" proof)

An agent cannot see the screen, so motion is verified mechanically: both suites
drive real animations over a **simulated clock** and assert the values move,
settle on target, and stop.

```sh
cargo test --test motion    # 10 tests on the motion primitives + velocity tracking
cargo test --test stack     # 8 tests driving the real live surface via egui_kittest
```

They catch the two failure modes that are otherwise invisible: *"the value never
moves"* and *"you forgot `request_repaint`, so it silently froze."* They also
assert the app **goes idle** once the animation lands, and that reduce-motion
truly collapses to zero frames.

The ones that pin down the corrected scope specifically:

- `entry_slides_in_from_the_anchored_edge` — asserts the card starts >300 px
  outside the frame, travels inward over several frames, and **overshoots past
  its home position before settling**. If the spring ever degrades into a plain
  ease-out, the overshoot assertion catches it.
- `flipping_the_anchor_mirrors_the_entry` — the direction is a parameter, not a
  hardcoded left.
- `dismissal_travels_toward_the_anchored_edge` — a dismissed card must fly
  *toward the edge*, not fade in place, and must actually leave the deck.
- `velocity_tracker_reads_a_throw_and_ignores_a_dead_stop` — a hard flick reads
  as a large velocity; a drag that **stopped before release reads as zero**.

### Headless snapshot test (the CI-story proof)

Proves an agent/CI can verify the UI with **no display server**, on any platform:

```sh
cargo test --test snapshot                 # renders offscreen (wgpu), diffs the baseline
UPDATE_SNAPSHOTS=1 cargo test --test snapshot   # regenerate the baseline PNG
```

Baseline: `tests/snapshots/quick_access.png`.

## Layout

```
src/theme.rs      design tokens (colour ramp, spacing, radii, elevation, type) + custom egui Style/Visuals
src/motion.rs     motion tokens (durations, easings, springs, stagger, velocity tracking,
                  live-tunable gesture physics, reduce-motion) — the D19 layer
src/paint.rs      hand-drawn primitives via egui::Painter (glass panels, shadows, scrims, buttons, shortcuts)
src/icons.rs      Tabler SVG -> resvg raster -> egui texture pipeline
src/surfaces.rs   the four faked surfaces (+ the Quick overlay's static stack/swipe/drag depictions)
src/stack.rs      the LIVE animated capture stack — anchored entry, 1:1 drag, velocity fling,
                  spring-back, deck reflow. This is the spike.
src/tuner.rs      the M overlay: spring/friction/threshold sliders, anchor flip, reduce-motion
src/app.rs        eframe shell, CLI config, key handling, repaint scheduling, self-capture
src/vibrancy.rs   native macOS material (window-vibrancy) — Liquid Glass / HUD vibrancy
src/main.rs       entry, arg parsing, transparent/borderless/on-top ViewportBuilder
assets/fonts/     Inter TTFs + OFL license
assets/icons/     the exact Tabler SVGs used + MIT license
tests/snapshot.rs headless egui_kittest wgpu snapshot
tests/motion.rs   headless frame-stepping tests for the motion primitives
tests/stack.rs    headless frame-stepping tests for the live surface
```

## Licensing of vendored assets

- **Inter** — SIL Open Font License (`assets/fonts/LICENSE.txt`).
- **Tabler Icons** — MIT (`assets/icons/LICENSE`). Only the icons actually used
  are vendored.

No competitor UI (CleanShot, Capso, …) is copied or committed here; those were
studied only to calibrate the quality bar.
