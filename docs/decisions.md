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
- **Drag-out from the capture stack** — the hero interaction (see D12)
- **Clipboard** — captures also land on the clipboard, ready to paste
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

**Note from the live CleanShot UI (2026-08-26) — corrected.** An earlier reading
of this concluded that CleanShot uses an adjustable corner-radius slider for
window captures. **That was wrong.** When the capture *is* a window, CleanShot
**disables Inset, Auto-balance, Shadow and Corners entirely**, showing
"These options are unavailable for window screenshots." The radius slider exists
only for non-window screenshots, where a plain rectangle is being composited onto
a background and the radius is genuinely a design choice.

**The real rule, and it is stronger:** for a window capture the OS output *is*
the truth. ScreenCaptureKit already returns correct corners, shadow and alpha,
so CleanShot does not post-process the geometry at all — that is why it is
flawless. Capso's unclean radii come from doing post-processing that should not
happen.

**Therefore, for Scrozz:** window captures are sacred. Take what the OS gives and
composite nothing. Where a platform does *not* give correct corners and alpha
(Windows, Linux), reconstructing them is a **fidelity gap to be closed**, held to
the acceptance criteria above and to golden-image tests — never an invitation to
apply an adjustable radius. Radius, shadow and inset controls apply only to
non-window captures.

---

## D17 — Competitor UI reference lives outside the repository

**Decision.** Competitor screenshots (CleanShot X, Capso) are kept at
`~/.copilot/scrozz-ui-reference/`, indexed by `INDEX.md`, and are **never
committed to this repository**. Documentation references them by path only.

Agents may study them to calibrate the *quality bar* — spacing rhythm, corner
radii, shadow softness, control density, information hierarchy, and which
options are worth exposing at all. Agents may **not** copy their designs, icons,
colour values, or layouts. Scrozz's visual design is original and uses Tabler
Icons (MIT).

**Why.** These are copyrighted product UI. A GPL-3.0 repository must stay clean
of them permanently. Keeping the library in a stable location outside any
worktree also means it survives branch switches and new worktrees, so every
future agent session can find it.

---

## D18 — Storage and sharing: any folder, plus the one thing folders can't do

**Decision.** Two distinct capabilities, both in the v1 plan:

1. **Arbitrary save location** — the export path accepts *any* mounted path:
   local folder, network/SMB share, or a synced folder (iCloud Drive, Dropbox,
   Google Drive, OneDrive). This is a hard requirement regardless of any cloud
   feature.
2. **S3-compatible upload → shareable URL** — bring-your-own bucket (R2, S3, B2,
   generic S3). No hosted service, no accounts, no bills to us.

**Hard architectural constraint:** saving and uploading happen **off the capture
path** — queued, asynchronous, with visible progress and retry. A capture must
never block on I/O. Writing straight to a slow SMB share on the capture thread
would stall the app at the worst possible moment.

**Why.** The maintainer's observation sharpens the scope: *"there's like local
storage and you point at a folder but you could just select a network drive and
then bam you're good to go… how ever would they bring it aside from that?"*

A configurable folder target genuinely delivers most of what "cloud" means —
captures land somewhere synced, on every device, on infrastructure the user
already pays for. And since a configurable save location is required anyway, that
capability is nearly free.

The **one thing a folder cannot do is produce a shareable URL at the moment of
capture.** A synced folder needs a manual trip to get a link. That single gap is
the entire justification for S3-compatible upload, and it is what CleanShot Cloud
actually sells.

Deliberately excluded: link expiry, password-protected links, and view analytics
all need a server or a viewer page — T3 at best, possibly never. Team management
is a permanent non-goal.

**Sequencing.** In the plan and designed for from the start so the storage
abstraction is never retrofitted; built after the core capture → annotate → drag
loop is solid.

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

## D12 — The capture stack is the primary interface; drag-out is the hero action

**Decision.** The post-capture overlay is a **stack of captures**, not a single
card. Its primary interaction is **dragging a capture directly into another
application** — above copy, above save. Swipe-to-dismiss is a primary gesture.
Clipboard remains essential but is no longer described as "first".

**Why.** Maintainer, on the behaviour that defines the app: *"you can swipe the
screenshots that stack in the bottom right down, and also drag them into a chat
or wherever you want to send them — which is SICK core functionality, almost
more intuitive than copying to clipboard."*

**Consequences.**
- The overlay's data model is a **collection**, not one capture. Deciding this
  late would be a rewrite of the surface used most.
- **Promised-file drag is v1 infrastructure, not polish.** Dragging a capture
  never written to disk needs `NSFilePromiseProvider` (macOS),
  `CFSTR_FILEDESCRIPTOR` delayed rendering (Windows), and XDND `text/uri-list`
  with a temp file (Linux, portal-mediated on Wayland). Three implementations,
  budgeted as real work.
- Swipe needs a **non-trackpad equivalent** on Windows and Linux.

---

## D13 — Accessibility: full commitment, canvas exempted

**Decision.** Ship **full accessibility**: screen-reader support across all
chrome, complete keyboard-only operation, honoring OS reduce-motion, contrast
and text-size settings, and WCAG AA contrast. The **annotation canvas is
exempt** from screen-reader semantics — it gets keyboard operation and a
structured layer list instead, the same compromise Figma makes.

**Why.** Accessibility and custom-drawn UI are **orthogonal**, not opposed —
this was initially framed wrongly. Assistive technology reads a *semantic tree*
the app publishes, never the pixels. **AccessKit** exists precisely for
"toolkits that render their own user interface elements," with shipping adapters
for macOS NSAccessibility, Windows UI Automation and Linux AT-SPI, under
MIT/Apache-2.0. Flutter, Figma and Chrome are all fully custom-drawn and
accessible.

Accessibility imposes only *design constraints* — contrast ratios, no
meaning-by-colour-alone, visible focus, scalable text — none of which force an
ugly result. Building the semantic tree from day one is far cheaper than
retrofitting it.

---

## D14 — Annotations are permanently editable; no user-facing project file

**Decision.** A **retained vector scene graph** from day one — every annotation
stays a live object. **No `.scrozz` project file is shipped.** Instead,
**capture history persists the full editable document automatically**, so any
past capture reopens with its annotations still editable. Exporting a PNG is a
*render*, not a save.

**Why.** The maintainer's requirement is that annotations are never permanent,
and his question — "what does the project file even do for the user?" — is the
right one. A project file solves editability by making the user manage files.
Persisting documents in history solves the same problem with **no file
management at all**, which is strictly better UX than CleanShot's `.cleanshot`
files.

Shipping a public format also means migrating it forever. Keeping serialization
internal and unadvertised means the format can change freely while tools are
still being designed. A user-facing export/import format can be added later,
once the tool set has stabilised, without breaking anyone.

---

## D15 — Attempt everything; gate permissions behind first use

**Decision.** No feature is cut for being hard — effort is not a constraint.
Features blocked by genuine platform limitations slip to v1.1 rather than being
abandoned. **But every invasive permission is requested at the moment a feature
is first used, never during onboarding or at launch.**

Applies to: Screen Recording, Accessibility, Input Monitoring (click and
keystroke overlays), Camera, Microphone, and Wayland RemoteDesktop.

**Why.** Maintainer: *"required effort should not stop us from trying."* Agreed
— with one correction. The problem with click and keystroke overlays was never
effort; it is **trust cost**. Requesting Input Monitoring — a keylogger-class
grant — during first-run onboarding taxes every user for a feature most never
use. Deferring the prompt to first use removes that tax entirely and makes
"attempt everything" safe. Nothing needs to be cut, only sequenced.

---

## D16 — Contract-first crates, and competitive implementation

**Decision.** A Cargo workspace monorepo, one crate per domain, developed
contract-first:

- **Phase 0:** a single agent defines every crate boundary as Rust traits plus
  golden test fixtures, with `todo!()` bodies. The whole architecture compiles;
  every test fails.
- **Phase 1+:** many agents fill in implementations in parallel, each owning
  exactly one crate, each verified by tests written before it started.

**Rules that prevent collisions:**
- One agent owns the workspace manifest and `Cargo.lock`; feature crates never
  edit them.
- Crate ownership is exclusive per task — never two agents in one crate.
- **Specs are agreed before implementation.** Where a spec is undefined, it gets
  defined and agreed first.

**Competitive implementation.** Where a component is high-stakes or ambiguous,
run **two agents against the same contract on separate branches and pick the
better result**. This is the maintainer's idea and it is a good one — but it
only works *because* of contract-first: the pre-written tests, benchmarks and
golden images make "better" an objective judgement. Without them it degenerates
into comparing vibes, and costs double for nothing.

**Why.** The limit on using unlimited agents is **interface stability, not agent
count**. Agents working against fixed contracts scale nearly linearly; agents
working against undefined boundaries collide and produce mush. Contract-first
inverts the usual failure mode — an agent cannot drift, because the contract and
the test already exist.

---

# Open questions

- **Q12 — UI stack (egui).** Provisional, pending the visual spike in
  `spikes/ui-spike/`. Decided on screenshots, not argument.
- **Onboarding and first-run flow.** Not yet designed. Interacts with D15's
  permission sequencing.
- **Scrozz design language.** Colour ramp, type scale, spacing, elevation, icon
  set (Tabler, MIT). Being explored by the spike.
