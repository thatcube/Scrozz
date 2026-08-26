# Scrozz Decisions

Decisions settled during the architecture design review. Each is binding until
explicitly revisited. Open questions live at the bottom.

**Related:** `cleanshot-parity.md` (feature inventory) · `research/` (inputs)

---

## D1 — Clean-room implementation; Capso is reference only

**Decision.** Scrozz is written from scratch. Capso may be studied for design
ideas and borrowed from conceptually where it makes sense, but it is never a
starting point and no Capso source is copied.

**Why.** Capso is BSL 1.1 — not open source until 2029, and its code cannot be
relicensed into a GPL project. It is also macOS-only Swift: nothing in its
capture, OCR, or recording layer ports to Windows or Linux. Beyond licensing,
the goal is the right architecture for *our* three-platform use case, which is
not the problem Capso was solving.

---

## D2 — GPL-3.0 with an App Store Exception, plus a trademark

**Decision.** License Scrozz under **GPL-3.0 with the same App Store Exception
used by Plozz**. Separately, claim **"Scrozz"** as a trademark and add a
`TRADEMARK.md`.

**Why.** Matches the family (Plozz: GPL-3.0 + App Store Exception; Mozz:
GPL-3.0 only). GPL means no one can ship a closed-source Scrozz — any
derivative must publish source. The exception costs nothing now and keeps
proprietary distribution channels legally open if they are ever wanted.

**Worth stating plainly:** "open source, but nobody may sell it" is not
achievable. The Open Source Definition §6 forbids restricting field of
endeavor, so a no-commercial-use clause makes software non-open-source by
definition — that is exactly the trade Capso made with BSL. GPL permits selling
but requires source disclosure, which removes the commercial incentive.
**Trademark, not license, is what actually prevents rebranding.**

---

## D3 — Cross-platform core from the first commit

**Decision.** The platform-abstracted core exists from commit 1. Windows and
Linux backends compile in CI from day 1, even while stubbed. macOS is the first
platform polished to shipping quality.

**Why.** macOS-first-then-port is how apps become permanently macOS-only —
platform assumptions get baked into the core before anyone notices. Compiling
all backends continuously catches that drift immediately. macOS ships first
because it is the maintainer's daily driver, and dogfooding is what gets the
app finished.

---

## D4 — v1 core scope

**Decision.** v1 is not full CleanShot parity. It is:

- Screenshot capture: area, window, fullscreen
- Screen recording: video and GIF
- **Clipboard-first** — captures land on the clipboard, ready to paste
- Annotation editor, held to a high quality bar
- Automatic compression to the best codec the destination accepts
- **Window capture with clean edges** (see D9)

Everything else in `cleanshot-parity.md` is post-v1 and accumulates over time.

**Why.** Agent capacity is not the constraint; verification attention is.
Unverified features are worse than absent ones — that is what "Capso has bugs"
means in practice. A feature is not done until it has an automated test *and*
has been used by the maintainer at least once.

---

## D5 — Agents implement and validate; the maintainer gates shipping

**Decision.** Agents write the code and build the automated validation that
proves it works. The maintainer verifies personally — by testing or by
inspection — before anything ships. **Accessibility is a core acceptance
criterion, not a later pass.**

Pre-release, iteration speed wins: breaking changes are free, nothing is
frozen, no migration burden. Post-release this inverts.

**Reconciling "do it right" with "don't slow down":** foundations get the slow,
correct treatment; features iterate fast and breakably. There are no users yet,
so churn costs nothing.

---

## D6 — One shared UI, custom appearance, native performance

**Decision.** A single custom-drawn UI across all three platforms, with the
Scrozz design language. **Native *performance* is the requirement; native
*appearance* is not.** Native OS integration patterns are used where they are
the right pattern — menu bar on macOS, tray elsewhere — as are native
permission prompts, file dialogs, and drag-and-drop.

**Why.** The surfaces used daily are the menu bar item, the quick-access
overlay after a capture, and the annotation editor. None of those need to look
native, and the overlay and canvas are custom-drawn under any toolkit anyway —
a native toolkit buys nothing there. Three native shells would mean
implementing most of the feature list three times, in three languages, with
only the macOS one personally verifiable.

**Evidence.** No open-source screenshot app uses the "shared core + native
per-platform shells" pattern. Cap (21k stars) is Tauri + Rust sidecars;
Flameshot (30.7k) and ksnip are monolithic Qt. Cap dropped Linux entirely.

---

## D7 — Direct download only at v1

**Decision.** Ship via GitHub Releases (plus a Homebrew cask on macOS). Stores
come much later, once the app is polished and has real users.

**Why.** Store review is a gate that slows iteration, and Flathub's sandbox
would force portal-only behavior earlier than necessary. Costs are known and
not blocking: Apple Developer Program is already held; Microsoft Store
registration is free; Flathub and Snap are free. Windows code signing
(~$200–400/yr, or Azure Trusted Signing) is the only genuinely new cost, and it
is only needed for direct download without SmartScreen warnings.

The Mac App Store is not a target — its sandbox would break global hotkeys,
input monitoring, scroll synthesis for scrolling capture, and desktop-icon
hiding. CleanShot X and Shottr both decline it for the same reason.

---

## D8 — Linux: GNOME and KDE fully; wlroots best-effort and documented

**Decision.** GNOME and KDE are fully supported through `xdg-desktop-portal`.
wlroots compositors (sway, Hyprland) get best-effort support with **explicitly
documented** gaps. X11 remains fully supported. Linux is a real target, not an
afterthought.

**Consequence — a hard API rule:** the capture layer exposes platform
capabilities by **query, never assumption**. Wayland's restrictions must not
leak into the core API as implicit expectations.

**Why.** Wayland has no protocol for window enumeration, and
`xdg-desktop-portal-wlr` does not implement `GlobalShortcuts` at all — full
wlroots parity is not achievable, so it is not promised. GNOME and KDE cover
the large majority, and both support ScreenCast restore tokens (no repeated
permission prompts) and the `RemoteDesktop` portal that scrolling capture
requires. On wlroots, global hotkeys work by binding a compositor shortcut to
the Scrozz CLI.

---

## D9 — Window capture is judged by output, not by technique

**Decision.** No technique is mandated. The **acceptance criteria** are binding:

1. Corner radius pixel-exact for the platform and OS version
2. Correct alpha — genuine transparency outside the window, no matte
3. No halo, fringe, or dark edge artifacts
4. Shadow captured as a separable layer: includable, omittable, replaceable
5. Correct at every display scale factor, including mixed-DPI multi-monitor

Each platform may reach these however works best, determined by experiment.
**Every criterion is covered by per-platform golden-image regression tests.**

**Why.** This is the specific quality gap between CleanShot X (flawless) and
Capso (unclean radii). The likely cause of Capso's problem is compositing a
rounded rectangle at a guessed radius rather than using the true value.
macOS ScreenCaptureKit provides correct corners and alpha directly; Windows
does not (DWM's extended frame bounds exclude the shadow, and Win11's radius
varies by version); Linux requires compositing. Since the technique differs per
platform, the *tests* are the real specification — and this class of silent
visual regression is exactly what agents can catch automatically.

---

## D10 — Clipboard-compatible everywhere, smallest size that looks untouched

**Decision.** Captures are offered to the clipboard in **multiple formats
simultaneously, with PNG always present**, so the receiving application picks
what it understands. Encoding defaults are content-aware:

- Text, UI, and flat-colour regions → lossless
- Photographic content → perceptually lossless (visually indistinguishable)
- PNG output losslessly optimised (oxipng-class)

**Compression is user-configurable**, with the default tuned so no one needs to
touch it.

**Why.** Pasting reliably into Slack, Figma, docs, and chat is the entire point
of the app, so PNG must always be on offer. All three OS clipboards support
multi-format offers, so modern codecs are used where supported at zero cost to
compatibility. "Best codec" means *smallest encoding the destination accepts
that shows no visible degradation* — not simply smallest. Lossy compression of
screenshots containing text is never a default.

---

## D11 — CLI/headless-first architecture

**Decision.** Every capability exists first as a headless command with
deterministic file output. The GUI links the same core and contains **no
capture, annotation, or encoding logic of its own**.

**Why.** Three independent reasons converge:

1. **Agents cannot see a screen.** A headless CLI with golden-image assertions
   is the only way agent-written work is verified in CI without a human.
2. **Linux requires it.** On sway/wlroots there is no `GlobalShortcuts` portal;
   the only way to bind a global hotkey is a compositor keybinding invoking the
   Scrozz CLI. The CLI is a platform requirement, not a convenience.
3. **It is a genuine differentiator.** Nothing in this category is scriptable.

---

# Open questions

- **Q11 — Accessibility commitment level.** Gates stack selection.
- **Stack selection.** Blocked on Q11.
- **Repository topology and agent-parallelism boundaries.** Blocked on stack.
- **Annotation document model and project file format.**
- **Recording scope for v1** — webcam PiP, system audio, click/keystroke
  overlays each carry permission and platform cost.
