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

1. **Quick Access Overlay** (primary) — the post-capture corner UI, modelled as a
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

That opens the interactive window on the **live capture stack** — a real,
animated, hand-drivable surface. Everything below moves; grab a card and throw
it.

### Keys

| Key | What it does |
| --- | --- |
| `1` `2` `3` `4` | Switch surface — quick access · menu · annotate · onboard |
| `V` | Cycle the Quick surface: **live** → stack → swipe → drag (the last three are the original *static* depictions, kept for comparison) |
| `N` | **Spawn a new capture** into the stack — plays the entry animation, and the cards beneath settle back on a spring |
| `Backspace` / `Delete` | **Dismiss the top card** — flings it out with momentum and fades it |
| `R` | **Replay** the current surface's entry animation (watch it as many times as you like) |
| `M` | **Motion tuner** overlay — see below |
| `L` | Light / dark |
| `G` | Drawn backdrop on / off |
| `Q` / `Esc` | Quit |

The same legend is drawn along the bottom of the window, so none of this
requires reading the source.

### Gestures (this is the part worth judging)

| Gesture | What should happen |
| --- | --- |
| **Hover a card** | The chrome reveals: scrim fades up, then Copy/Save pills and the four corner icons fade in *with a slight upward rise, staggered a few ms apart*. Mouse out reverses it. |
| **Hover a button / pill / menu row** | Background washes in, icon tint warms. |
| **Press a button / pill** | Snappy scale-down, released on mouse-up. |
| **Press and hold a card** | Lift + scale — the grab cue that says *this is draggable*. |
| **Drag a card** | It follows the pointer with a little lag and inertia, tilting into the direction of travel; the deck beneath reflows. |
| **Flick a card away** | Velocity-based. A fast flick (> 520 px/s) throws it out with momentum, spin and a fade. A slow drag that never passes ~96 px springs back home. |

### Motion tuner (`M`)

A live overlay for dialling in the feel without a rebuild:

- **Duration multiplier** slider, 0.25× – 3×, with presets and a live readout of
  what `FAST` / `BASE` / `SLOW` currently resolve to in ms.
- **Easing override** dropdown — force every timeline onto one curve and watch
  the difference, with a preview graph and a dot sweeping the curve in real time.
- **Reduce motion** checkbox — the accessibility path (D13). Every duration
  collapses to zero; nothing animates, everything still works.
- **Replay / New capture / Dismiss** buttons, so the overlay is self-sufficient.

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
cargo test --test motion    # 8 tests on the motion primitives
cargo test --test stack     # 5 tests driving the real live surface via egui_kittest
```

They catch the two failure modes that are otherwise invisible: *"the value never
moves"* and *"you forgot `request_repaint`, so it silently froze."* They also
assert the app **goes idle** once the animation lands, and that reduce-motion
truly collapses to zero frames.

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
src/motion.rs     motion tokens (durations, easings, springs, stagger, reduce-motion) — the D19 layer
src/paint.rs      hand-drawn primitives via egui::Painter (glass panels, shadows, scrims, buttons, shortcuts)
src/icons.rs      Tabler SVG -> resvg raster -> egui texture pipeline
src/surfaces.rs   the four faked surfaces (+ the Quick overlay's static stack/swipe/drag depictions)
src/stack.rs      the LIVE animated capture stack — hover reveal, grab, drag, fling, entry
src/tuner.rs      the M overlay: duration multiplier, easing override, reduce-motion, replay
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
