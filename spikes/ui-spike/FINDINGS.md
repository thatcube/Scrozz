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
- **This environment ships a *patched* eframe/egui 0.36.1 with a non-standard `App`
  trait** (`fn ui(&mut self, ui, frame)` instead of the upstream `update(ctx, frame)`,
  plus `all_styles_mut` / `set_style_of` instead of `set_style`, and a `clear_color`
  override for transparency). None of it was hard once discovered, but **it is a
  standing risk for Scrozz**: whatever fork this is, it diverges from upstream egui's
  documented API, so upstream examples/plugins won't drop in unmodified and version
  bumps could hurt. Worth understanding *before* committing the whole app to it.

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

## 5. Animation / transitions — what did it cost to hand-roll?

**Not proven for real motion, and I want to be upfront rather than bluff it.** The
deliverable is static pixels, rendered deterministically for reproducible screenshots —
I did *not* build animated transitions, so this spike does **not** demonstrate motion.
What the drag-first surfaces *do* show is that gestures can be made to **read in a
still**: the swipe is depicted with an opaque flung card plus fading ghost echoes along
its path, and the drag with a lifted card, a drag-count badge, a grab cursor, and a
motion trail. That's a static depiction of motion, not motion itself — worth being
precise about.

What I can say from the primitives: egui is immediate-mode and repaints every frame, so
value-based animation (egui ships `animate_bool`/`animate_value_with_time` easing
helpers) is straightforward — hover fades, press sinks, a card slide-in are cheap to
hand-roll and would be a few lines each. A swipe-to-dismiss fling or a drag follow is a
position lerp, also cheap. The thing that would *cost* is anything gradient- or
blur-based in motion, or **rotation in motion** (§1 — no gradient primitive, no real
backdrop blur, no transform for images/text), which needs custom shaders or a
render-to-texture path. Static polish: proven. Simple motion: plausible and cheap.
Rotated/blurred motion: **unproven and not cheap** — if it matters, it deserves its own
small spike.

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
2. **Real macOS Liquid Glass behind live content needs native `NSView` work** — an
   eframe/winit patch or a custom view hierarchy. This is the one item that is *not*
   cheap and *not* a library call. If "true OS glass over the live desktop" is a
   must-have identity feature, budget a dedicated native spike for it; if a drawn
   glass card over the captured image is acceptable (it looks great — `quick_stack_dark.png`),
   you already have it.
3. **This patched eframe fork diverges from upstream egui's API.** Understand and pin
   it deliberately; it affects every future upgrade and every third-party egui example.
4. **No gradient primitive, grayscale text AA.** Neither blocks a premium look on
   Retina; both are papercuts worth knowing.

If the maintainer's fear was "egui is inherently ugly," this spike **disproves it** —
the pixels are premium. The honest amendment is: egui isn't ugly, it's *bare*. It ships
you a fast, cross-platform, headlessly-testable canvas and hands you the entire visual
design as *your* job. Scrozz is small and opinionated enough that that trade is a
**yes** — with the Liquid-Glass-behind-content caveat flagged in bright red.
