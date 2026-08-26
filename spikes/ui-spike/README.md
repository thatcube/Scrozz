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
rests on live in **[`screenshots/`](./screenshots/)**.

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
# Rust is user-local via rustup; each shell must source the env first.
source "$HOME/.cargo/env"

cargo run                     # interactive window, all three surfaces
```

Interactive hotkeys: **1 / 2 / 3 / 4** switch surface · **V** cycle the Quick
overlay state (stack → swipe → drag) · **L** toggle light/dark · **G** toggle the
drawn backdrop · **Q / Esc** quit. A small legend is drawn in the window.

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
`--variant stack|swipe|drag` (Quick surface only) · `--theme dark|light` ·
`--backdrop on|off` (the in-egui drawn wallpaper) ·
`--material none|vibrancy|glass` (native macOS material — see FINDINGS) ·
`--shot <path>` (render one frame to PNG and exit).

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
src/paint.rs      hand-drawn primitives via egui::Painter (glass panels, shadows, scrims, buttons, shortcuts)
src/icons.rs      Tabler SVG -> resvg raster -> egui texture pipeline
src/surfaces.rs   the four faked surfaces (+ the Quick overlay's stack/swipe/drag states)
src/app.rs        eframe shell, CLI config, self-capture
src/vibrancy.rs   native macOS material (window-vibrancy) — Liquid Glass / HUD vibrancy
src/main.rs       entry, arg parsing, transparent/borderless/on-top ViewportBuilder
assets/fonts/     Inter TTFs + OFL license
assets/icons/     the exact Tabler SVGs used + MIT license
tests/snapshot.rs headless egui_kittest wgpu snapshot
```

## Licensing of vendored assets

- **Inter** — SIL Open Font License (`assets/fonts/LICENSE.txt`).
- **Tabler Icons** — MIT (`assets/icons/LICENSE`). Only the icons actually used
  are vendored.

No competitor UI (CleanShot, Capso, …) is copied or committed here; those were
studied only to calibrate the quality bar.
