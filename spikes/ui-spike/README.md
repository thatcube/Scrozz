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

Three surfaces, built with a bespoke dark-glass design-token theme (no default
egui styling), real vendored fonts (Inter) and real vendored SVG icons (Tabler,
rasterised through `resvg` — no emoji/glyph substitutes):

1. **Quick Access Overlay** (primary) — the post-capture floating card: thumbnail,
   caption, and an action bar (drag-handle · copy · save · annotate · pin ·
   cloud-upload · close). Dark and light variants.
2. **Menu-bar dropdown** — capture-mode list with right-aligned ⇧⌘ shortcut
   hints and section dividers.
3. **Annotation toolbar** — crop · arrow · rectangle · ellipse · line · text ·
   highlighter · blur · pixelate · counter · pencil, plus a colour swatch,
   stroke-width control, and undo/redo, with selected / hover / default states.

## Run it

```sh
# Rust is user-local via rustup; each shell must source the env first.
source "$HOME/.cargo/env"

cargo run                     # interactive window, all three surfaces
```

Interactive hotkeys: **1 / 2 / 3** switch surface · **L** toggle light/dark ·
**G** toggle the drawn backdrop · **Q / Esc** quit. A small legend is drawn in
the window.

### Reproduce a screenshot

Each PNG in `screenshots/` is a capture of the **real running window** (so the
transparency/compositing is genuine, not mocked). The app can render one surface
to a borderless on-top window and self-capture:

```sh
cargo run -- --surface quick    --theme dark  --backdrop on  --shot screenshots/quick_dark.png
cargo run -- --surface quick    --theme light --backdrop on  --shot screenshots/quick_light.png
cargo run -- --surface menu     --theme dark  --backdrop on  --shot screenshots/menu_dark.png
cargo run -- --surface annotate --theme dark  --backdrop on  --shot screenshots/annotate_dark.png
```

Flags: `--surface quick|menu|annotate` · `--theme dark|light` ·
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
src/surfaces.rs   the three faked surfaces
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
