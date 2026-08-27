# Scrozz Decisions

Decisions settled during the architecture design review. Each is binding until
explicitly revisited. Open questions live at the bottom.

**Related:** `feature-audit.md` (feature inventory) · `research/` (inputs)

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

Everything else in `feature-audit.md` is post-v1 and accumulates over time.

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

This governs capture and export pixels, not transient UI around a preview. The
floating stack normalizes every provenance into the same fixed 210 × 150 point
container. Each image cover-fills that frame and is centre-cropped and clipped
to the shared radius. Cropping and chrome exist only in the thumbnail texture
mapping; they are never written back into the capture.

**Corollary — the concentric radius rule.** Wherever a rounded shape nests inside
another, `inner_radius = outer_radius − padding`. Violating it makes corners look
subtly wrong even when both shapes are "rounded". This belongs in the design
token layer as an enforced relationship, not a per-surface judgement call.

**Field evidence, 2026-08-26.** The UI spike shipped exactly this class of bug on
its first pass: a caption scrim painted as an *unrounded* rectangle over a rounded
thumbnail, squaring off the bottom corners. The maintainer spotted it immediately
in a screenshot review — which is the whole argument for D9 in miniature. Corner
defects are invisible to the person who wrote the code and glaring to everyone
else, so **anything painted over a rounded shape** (scrims, gradients, hover
fills, pressed and selected states, dividers) must respect that shape's geometry,
and golden-image tests must assert it at every interaction state rather than only
at rest.

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

**Decision.** The post-capture overlay is a **vertical list of captures**, not a
single card. Its primary interaction is **dragging a capture directly into
another application** — above copy, above save. Swipe-to-dismiss is a primary
gesture. Clipboard remains essential but is no longer described as "first".

**Layout.** Fixed slots anchored at the bottom-left; a new capture slides in from
off-screen left into the next empty slot upward, building a tower with the oldest
at the bottom. **Existing cards do not move on arrival.** Nothing ever covers
anything — zero overlap, full size and opacity, consistent gaps. Each card is
independently hoverable, draggable and dismissable; hovering one reveals only its
own chrome. Full layout, overflow and gesture rules are in D21.

> Wherever these documents say captures "stack", it means a **physical tower of
> discrete blocks** — accumulating in fixed slots, each fully visible — never
> overlapping or occluding. Earlier drafts described both an overlapping
> card-stack and a reflowing list; both were wrong.

**Motion (see D19).** A new capture slides in from the anchored screen edge and
takes the slot nearest the anchor corner; existing cards shift away with a spring
settle. Dismissing a card lets its neighbours close the gap. The list reflows —
it never re-stacks.

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

**Chrome model — at rest the capture is the only interface.** A card at rest
shows the image and nothing else: no icons, no buttons, no handle. **On hover** a
scrim fades in with **Copy above Save in a vertical pair of prominent pill
buttons** plus four small secondary icons at the corners (pin, close, annotate,
upload). Close follows the host convention: top-left on macOS and top-right on
Windows and Linux.

This resolves cleanly against drag-first rather than conflicting with it: **the
card itself is the drag handle**, so the hero interaction needs no chrome at all.
Grabbability is communicated through cursor change and a subtle lift on press,
never a visible handle. Copy and Save are the primary *buttons*; drag is the
primary *gesture*; they occupy different channels and do not compete.

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

## D19 — Motion is part of the design system, not decoration

**Decision.** Micro-interactions and animation are a **product requirement**.
Motion lives in the shared token layer alongside colour, spacing and type —
named duration tokens, named easing curves (including a spring), and per-element
animation state — never ad-hoc interpolation scattered through drawing code.

**The governing principle: motion belongs to objects that move through space;
controls respond instantly.**

- **Animate — the capture cards.** A new capture **slides in from the anchored
  screen edge** and the list springs to make room. A card is **grabbable and
  follows the pointer 1:1**, tilts while moving, and **flings off toward that
  edge with real momentum** past a velocity or distance threshold — or **springs
  back** below it. Neighbours settle when one leaves. Velocity tracking is
  essential: a dismissal that ignores throw speed feels dead.
- **Do not animate — buttons, pills, menu rows.** Hover and press are **instant**
  state changes. No fade, no scale, no easing.

**Why the split.** Maintainer: *"i think the button animations dont NEED to be
anything, the instant background changes are GOOD actually. make it feel more
responsive."* He is right, and it corrects an earlier draft of this decision that
called for animated button feedback. Easing a control makes an app feel
*sluggish*; instant feedback makes it feel *responsive*. Physical motion on
spatial objects is what makes it feel *alive*. Spending motion budget on controls
actively degrades the product.

**Accessibility gate (per D13):** the OS reduce-motion setting collapses every
duration to zero. Motion is never load-bearing — it must not be the only carrier
of meaning.

**Why motion at all.** Maintainer: *"things like micro-interactions and
animations are also really essential IMO to building a great app."* This is the
difference between an app that works and an app that feels good, and it is judged
in the first ten seconds of use.

**Known cost, deliberately accepted.** egui has **no built-in animation system** —
only `animate_bool_with_time` / `animate_value_with_time` primitives. Easing,
springs and staggering must be built. epaint also has **no rotation primitive**,
so a tilting card is hand-built from rotated polygons. That is precisely why it is
being proven in a spike rather than assumed: a stack that cannot animate well
fails a stated requirement, and better to learn that now than in month three.

**Implementation note.** egui is immediate-mode and only repaints on input, so an
animation silently does nothing unless `ctx.request_repaint()` is called while it
is in flight — and repainting unconditionally pins the CPU, which would undermine
the native-performance argument that justified choosing egui. Animate, then go
idle.

**Consequence for review.** Still screenshots cannot convey feel. Any UI work
that changes interaction must be reviewed by **running it**, not by looking at
stills — the first spike produced convincing static *depictions* of motion while
containing no motion at all.

---

## D20 — The capture dock

**Decision.** Swiping the capture list **down** collapses it into a **dock**: a
short, wide box at the **bottom-left of the screen**, the **same width as a
capture card** but roughly **one-sixth the height**, carrying an **upward
chevron**. **Clicking it or swiping up** brings the captures back.

**Swiping down collapses — it never dismisses.** Nothing is lost; the captures
are still there, just out of the way.

**Motion.** Collapse and expand are headline animations. Cards must visibly
travel *into* the dock and *out* of it — a fade would be wrong. The spatial
relationship between the dock and the captures has to be legible.

**Why.** Maintainer's design. It solves a real problem CleanShot doesn't: the
overlay is in the way, but dealing with every capture individually just to clear
the screen is friction. The dock gets the whole list out of the way in one
gesture and costs nothing to bring back.

This is a **beyond-parity differentiator** — CleanShot has "temporarily hide
overlays" as a settings toggle, not a spatial, reversible, one-gesture affordance.

---

## D21 — The overlay interaction and animation set

### Layout: fixed slots, nothing reflows on arrival

The overlay is a column of **fixed slots** anchored at the bottom-left. A new
capture slides in from off-screen left into the **next empty slot upward** —
building a tower, oldest at the bottom. **Existing cards do not move.** No
shifting, no reflow, no settle when a capture arrives.

The slot count is **computed from display height** (≈6 on a 16" MacBook Pro),
recomputed when the display changes. Never hardcoded.

**Overflow.** When a capture arrives with every slot full, the **oldest card
slides out to the left using the same exit animation as a manual dismiss** —
maintainer: *"almost like you closed it yourself or swiped it yourself."* Nothing
is lost; it remains in history (D14).

**Departure.** Cards above a vacated slot fall down to close the gap. Arrival
moves nothing; departure obeys gravity.

### Gestures: direction is intent

| Gesture on a card | Meaning | Behaviour |
|---|---|---|
| **Left** | Dismiss | Velocity-driven fling off-screen; springs back below threshold |
| **Right or Up** | **Drag onto something** | Enters drag mode; on release the card drops and animates away; cancel springs it back |
| **Down** | Collapse | The whole list collapses into the dock (D20); non-destructive |

**Consequence: there is no drag handle and no drag button.** Direction alone
expresses intent — throw it left to discard, push it right or up to hand it to
another app, push it down to hide everything. This is what resolves D12's
drag-first requirement against D12's "no chrome at rest" requirement.

Drag mode is where the OS promised-file drag lives (D12), so a capture never
written to disk can still be dropped into another application.

### The animation set

1. **Card enters** — slides in from off-screen left into the next slot up.
2. **Card leaves** — via close, copy, save, or swipe-left. **One shared exit
   animation**, also used for overflow retirement.
3. **Cards above fall down** to close the gap.
4. **Drag mode** — lift on right/up, hand off and animate away on release,
   spring back on cancel.
5. **Dock collapse and expand** (D20).

**Button press animation is explicitly optional** and lowest priority.

**The mental model is phone notifications:** discrete cards that accumulate, get
swiped away individually, and can be collapsed out of the way.

**Why copy and save also dismiss.** That is the real workflow — capture, copy,
gone. A card left on screen after its action has been taken is clutter.

**Why so few animations.** Maintainer: *"im realizing theres not that much
animation… it might be nice to animate button presses for example, but that's not
a hard requirement."* Motion concentrated on the few moments that carry the
product's feel beats motion sprinkled everywhere — and per D19, easing controls
actively makes an app feel slower.

---

## D22 — The UI stack is Rust + egui/eframe

**Decision.** Scrozz's UI is built on **Rust + egui/eframe**, custom-drawn and
shared across all three platforms. Versions pinned exactly — egui is 0.x and
churns between minor releases.

**Why it won.** It is the only option satisfying every constraint at once:

- **Native performance (D6).** Instant startup — measured "nearly instant" on an
  unoptimised debug build. Tauri's 300–800 ms WebView cold start disqualifies it
  for a hotkey-driven capture app.
- **Real transparent, click-through, always-on-top overlays** on every platform —
  the app's most critical window type. Tauri supports only whole-window
  click-through; Slint has no transparent overlay support at all.
- **Accessibility (D13)** via AccessKit, which exists precisely for toolkits that
  draw their own widgets.
- **Headless verification (D5, D11).** `egui_kittest` renders offscreen through
  wgpu with **no display server** and diffs against committed PNG baselines —
  proven working in the spike. This is what lets agents verify their own UI work.
- **One language** shared with the capture layer.

**Decided on evidence, not argument.** A throwaway spike (`spikes/ui-spike/`)
built the real surfaces with a custom token layer, vendored Tabler icons and
macOS Liquid Glass. The objection was aesthetic — *"egui is pretty ugly… that is
gonna need some work"* — and the pixels answered it: *"this is all looking very
good."* Motion, the last open risk, was retired separately once egui was
confirmed to animate well.

**The honest characterisation, from the spike's own findings:** *"egui isn't
ugly, it's bare."* It is a beautiful **canvas**, not a beautiful **widget kit**.
The polish does not live in egui; it lives in the token layer you bring. Rerun's
`re_ui` took exactly this path.

**Costs accepted, with their consequences:**

1. **The entire control library is hand-built.** No stock components to lean on.
   Phase 0 must therefore produce a **shared widget library**, not per-surface
   drawing — the spike's `paint.rs` hit 446 lines for three surfaces, and that
   only scales through a real component layer.
2. **No gradient primitive in epaint.** Gradients are stacked translucent shapes;
   a moving gradient would need a shader.
3. **No rotation primitive** — not for images, rounded rects or text. Tilt is
   hand-built from rotated polygons, and **rotated text is unachievable** without
   a render-to-texture path. Design around it.
4. **Grayscale text antialiasing**, not subpixel. Indistinguishable from native on
   Retina, slightly softer at 1× — relevant to 1080p Windows and Linux users.
5. **True Liquid Glass behind *live desktop content*** needs native `NSView` work,
   not a library call. Glass drawn over the captured image looks excellent and is
   already achieved; genuine OS glass over the live desktop deserves its own spike
   if it becomes an identity requirement.
6. **~~The eframe build diverges from upstream.~~ Investigated and false.** eframe
   `0.36.1` is the genuine latest crates.io release and `fn ui(&mut self, ui, frame)`
   *is* its real upstream `App` trait. The spike misread a crate newer than its own
   training data as a patched environment. Nothing to resolve — see Open Questions.

---

## D23 — History retention

**Decision.** **Annotation documents are kept forever** — they are kilobytes, and
keeping them is what makes D14's "annotations are never permanent" actually true.
**Source images are evicted against a size cap**, default ~10 GB and
user-configurable, oldest first. **Pinned captures are never evicted.**

**Why.** Documents are tiny and carry all the value; images are the bulk. Evicting
only images preserves the promise at negligible cost. CleanShot's equivalent is a
blunt time slider — Never / 1 day / 3 days / 1 week / 1 month — which throws away
the edit history along with the pixels.

---

## D24 — Competitor names appear only in comparison documentation

**Decision.** The names of other screenshot tools appear in exactly **two** places:
this repository's `docs/` audit material, and a single public comparison page.
They appear **nowhere else** — not in product copy, not in the UI, not in feature
names, not in issue titles, not in code comments, not in commit messages, not in
store listings or the README's own description of what Scrozz is.

Everywhere downstream of the audit, features are referred to by **their Scrozz
names**. An issue says "the capture dock collapses on downward swipe," never
"like CleanShot's tray."

**Why the strict version.** The loose version — "keep it out of marketing" — fails
where it matters. Under D1 this is a clean-room build, and D5 means agents write
most of it. An agent that reads "match CleanShot's magnifier" in an issue will
reach for the competitor's behaviour as the specification. That is how a clean-room
design quietly stops being one, and how you ship a worse copy of someone else's app
instead of a better one of your own. Confining competitor names to the audit forces
every downstream artifact to state what Scrozz should *do*, which is the only thing
an implementer can actually build from.

The positioning, stated plainly: **CleanShot X is the bar. It is also macOS-only.
Scrozz is macOS, Windows and Linux — and better.** Steal like an artist: take the
best ideas, name them ourselves, beat them.

**The comparison page.** Modelled on the one Capso publishes. Candidate rows:
screenshots, all-in-one capture HUD, recording, webcam PiP, OCR, annotation,
pin-to-screen, beautification, cross-platform, open source, price. Three binding
constraints:

1. **It must be dated and factually accurate.** Naming a competitor in a comparison
   is nominative fair use and is entirely legitimate; an out-of-date or wrong table
   is the one way to turn that into a real complaint. Regenerate it each release.
2. **No competitor logos or wordmarks** — plain text names only, no styling that
   implies endorsement or affiliation.
3. **At least one row we lose.** A table that wins everything is marketing and
   reads as such. On day one CleanShot X will beat Scrozz on macOS polish, and
   saying so is what makes the rows we do win believable.

---

## D25 — Every screenshot is generated; none is taken by hand

**Decision.** Scrozz ships a **screenshot generator** built on the headless
`egui_kittest` harness the spike proved out. It boots any UI surface with seeded
state, renders it with no display server, and writes PNGs. It is the **only**
source of product imagery. Nobody ever hand-captures a screenshot of Scrozz —
which would be a delicious failure mode for a screenshot app.

Three consumers, one harness, differing only in output profile:

| Consumer | Profile |
|---|---|
| **Golden-image tests** | Fixed scale, committed baselines, CI fails on pixel diff |
| **Store assets** | Exact per-store pixel dimensions, 2×/3×, localised |
| **README and docs** | Annotated stills, plus animated captures for motion |

**Why this is the important one.** It is the mechanism that makes agentic UI work
possible at all. An agent cannot see. The spike demonstrated the failure exactly:
it painted an unrounded scrim over a rounded thumbnail, squaring the bottom
corners — the precise defect class D9 exists to prevent — and never noticed, while
Brandon caught it in seconds. Golden images convert *"a human has to look at it"*
into *"the build fails."* Without that, every visual regression needs a human; with
it, agents can work the UI unattended and D5 holds.

The store-asset payoff is separate and also large: per-store pixel dimensions are
rigid, and producing them is a miserable manual chore once per release per store
per locale. Generated, they are free, they are always of the **real** UI rather
than a mockup, and **they cannot go stale** — they regenerate from the same code
that ships.

**What this requires, and these are hard requirements:**

- **Determinism.** Fixed RNG seed, frozen clock, pinned font set, no live desktop
  content behind glass, no real filesystem timestamps. A flaky golden test gets
  disabled within a week and then the whole apparatus is worthless.
- **A virtual clock.** D19 and D21 make Scrozz motion-heavy, so the harness must
  render *a named instant* — "card-enter at t=180ms" — not whatever frame it
  happened to catch. This same frame-stepping is what produces animated assets:
  step the clock, dump N frames, encode. Stores accept app-preview video and
  GitHub renders animated WebP.
- **Named state fixtures.** "Six cards stacked," "dock collapsed," "annotation
  toolbar open," "overflow evicting the oldest card." These serve as the golden
  corpus *and* the marketing scenarios — one list, maintained once.
- **Sizes as data.** A manifest of store targets, so supporting a new store is a
  config entry rather than a code change.

Localisation falls out for free: every locale, every store, one command.

---

## D26 — Onboarding teaches only what the app cannot teach itself

**Decision.** Build the onboarding **flow** now; defer its **visuals** until the
real UI exists. It is skippable, and re-runnable from settings.

D15 already forbids the usual design: permissions are requested at first use of the
feature that needs them, never up front, so onboarding is explicitly **not** a
permission wizard. That constraint removes most of what onboarding normally
contains, and what remains is small and genuinely necessary — the things that are
invisible until someone points at them:

1. **The drag-out gesture.** D12's hero action. Brandon called it "almost more
   intuitive than copying" — but only *once you know it is there*. Nothing on
   screen announces that a capture card can be dragged straight into another app.
   This is the single highest-value thing onboarding does.
2. **The capture hotkey** — confirm the default or set your own.
3. **Where captures go** — D18's any-folder choice.
4. **Linux/wlroots only:** hotkeys require a compositor keybinding. Explain it and
   generate the config line to paste. Per D11 this is not a nicety; on wlroots it
   is the only hotkey path that exists, and a user who is not told this concludes
   the app is broken.

Everything else the app teaches in place, at the moment it becomes relevant.

**Why deferring the visuals costs nothing.** Each screen is a sentence and one
animation, and under D25 that animation is *generated from the real UI* by the
screenshot harness. Drawing onboarding art before the UI exists would mean drawing
it twice and having the second version disagree with the product. Building the flow
first and generating its imagery last is strictly cheaper and cannot drift.

---

## D27 — The app is invisible at rest; only captures appear

**Decision.** Scrozz's home is the **menu bar (macOS) / system tray (Windows,
Linux)**. At rest the app shows **nothing**: no window, no Dock icon, no taskbar
entry. The only thing that ever appears unbidden is a **capture card**.

Every Scrozz surface falls into exactly one of three classes, and the class
determines its window behaviour completely:

| Class | Surfaces | Window behaviour |
|---|---|---|
| **Invisible** | tray/menu-bar item, hotkey listener, CLI | No window at all |
| **Transient floating** | capture cards, capture dock, selection overlay, magnifier, pinned captures | Borderless, always-on-top, **fixed position — not user-movable**, dismissible |
| **Ordinary window** | settings, annotation editor | **Native, movable, resizable, standard chrome** |

**Why the middle class is not movable, which reverses my earlier position.** I
first concluded that every floating window must be draggable, having watched an
undraggable always-on-top window make this machine unusable. That was the wrong
lesson from the right incident. The actual failure was not immovability — it was
that **a large window existed at all when nothing should have been on screen.**
Capture cards live in fixed slots because the slot *is* their meaning: position
encodes recency, and letting the user scatter them destroys the ordering that
makes the pile readable. CleanShot's cards are not free-floating either.

**What actually prevents the failure**, then, is not a drag handle but three
properties every transient surface must have:

1. **It is small.** A capture card is a thumbnail, not a panel. Nothing in this
   class ever covers a meaningful part of the screen.
2. **It is escapable without documentation.** Swipe left dismisses, swipe down
   collapses to the dock (D20), Escape clears everything. A user who is mid-task
   and annoyed must be able to make it go away on the first guess.
3. **It never blocks what is beneath it.** Empty space between cards passes
   clicks through, so the surface is only "on top" where there is actually
   something to see.

The selection overlay is the sole exception to "small": it is deliberately
fullscreen, and it is also deliberately momentary — it exists between the hotkey
and the click, and Escape always cancels it. Before that surface is hidden for
capture it must first become mouse-pass-through, and the card renderer must
reassert its own input region when it returns. An invisible fullscreen window
that can still consume even one click violates this decision, regardless of
whether a capture is still being processed. Keyboard ownership may remain only
long enough to consume the key-up that committed or cancelled selection; pointer
input is released independently and immediately.

Outside active selection, Scrozz's visible UI should not blink merely to keep
itself out of a screenshot. A capture backend that can exclude the current
process must do so and declare that guarantee; the cards then remain visible to
the user while native capture reconstructs the pixels behind them. Backends
without that guarantee retain the fail-safe hide/capture/restore path rather
than leaking Scrozz chrome into the image.

**Ordinary windows are genuinely ordinary.** Settings and the annotation editor
are real, native, movable, resizable windows with normal chrome. They are opened
deliberately, they are long-lived, and they must behave exactly like every other
window on the system, including tiling, mission control, snapping and window
management shortcuts. Nothing is gained by making these custom, and everything is
lost.

**Consequence for development.** Any spike or debug build that creates a floating
window starts at `WindowLevel::Normal`, makes always-on-top an explicit opt-in
toggle with a visible legend, and closes its window when finished rather than
leaving it on a developer's desktop between runs. The general principle stands
even where drag does not: **the more insistent a window is, the cheaper its
escape must be.**

## D28 — The capture stack is bottom-anchored and grows upward

**Decision.** The pile of capture cards is anchored to the **bottom-left of the
screen** and grows **upward**. Cards enter and leave **only from the left**, and
they only ever move **downward**.

```
   slot 5  ← 6th capture
   slot 4
   slot 3
   slot 2
   slot 1  ← 2nd capture
   slot 0  ← 1st capture, and the first to leave
   ─────── bottom-left of the work area
```

The complete behaviour, which is the whole specification:

1. **First capture** appears at slot 0, the bottom, sliding in from the left.
2. **Each subsequent capture** slides in from the left into the next slot up.
   **Existing cards do not move at all** while the pile is still growing.
3. **When the pile is full**, three things happen as one coordinated motion: the
   oldest card at slot 0 slides out to the **left** — the same way it came in —
   every remaining card **falls down** one slot, and the new card slides in from
   the left into the top slot.
4. **Dismissing any card** applies the same gravity: cards above it fall down one
   slot to close the gap; cards below it never move.

**The invariant, which is the thing to check an implementation against: a card
never moves upward.** Upward change happens only when a new card arrives at a new
top slot, and even then it is the arriving card, not existing cards shifting.

**Slot count** is derived from the available work-area height, not hard-coded.
Six is the target on a 16-inch MacBook Pro and matches the practical ceiling of
comparable tools. It must clamp sensibly on small displays.

**Settled geometry is exact:** every card is 210 × 150 points, adjacent cards
have an 8-point visible gap, the shared left edge is 40 points from the work
area, and the bottom card is 8 points above the Dock or taskbar. The native
overlay window is re-anchored to the operating system's live work area after any
selector or display transition; window-manager placement hints are not accepted
as proof that the Dock has been excluded.

**Why bottom-anchored.** Position encodes recency, and a bottom anchor makes the
pile physical: things accumulate on top of each other and settle downward under
gravity, which needs no explanation. A top-anchored pile behaves like phone
notifications, where every arrival shoves the existing ones down — motion the
user did not ask for, applied to cards they may be reaching toward. The bottom
anchor keeps existing cards perfectly still until something actually leaves,
which is the property that makes the pile safe to aim at.

**Possible future setting.** Top-anchored may be offered as a preference later.
It is not the default and is not built for v1.

## D29 — Annotation semantics that users can observe

**Decision.** Four rules emerged from implementing the annotation model. Each is
recorded here rather than in the crate because each one is *visible to the user*,
so changing it later would change behaviour people had come to rely on.

1. **Redaction is destructive, and destroys what is beneath it in z-order.**
   Blur, pixelate and solid burn into the pixels during the render pass. A
   redaction placed above an arrow destroys that arrow's pixels too; annotations
   placed *after* it still draw on top. This is the only correct reading — a
   redaction that quietly spared some content beneath it would be a privacy
   failure wearing the appearance of one.

   The implementation goes further in one respect worth keeping: **blur samples
   clamp-to-edge from the whole image**, not from the region in isolation. A
   region blurred in isolation darkens at its edges, which visibly advertises
   exactly where the redaction is and how big the hidden content was.

2. **Counters renumber by creation order, never by z-order.** Raising a numbered
   step marker must not resequence the steps. The number is the user's meaning;
   z-order is presentation, and presentation must not rewrite meaning.

3. **A solid redaction falls back to opaque black if its style would render it
   invisible.** A see-through redaction is the worst possible outcome, so the
   failure mode is deliberately biased toward hiding too much.

4. **Rendered output is `RgbaPremultiplied8`.** Un-premultiplying would lose
   precision in exactly the low-alpha edge pixels that D9 exists to protect.
   Downstream encoders must handle this — `scrozz-export` un-premultiplies once,
   at the point of encoding to a straight-alpha format, and nowhere else.

**Also settled, and less contentious:** annotations live behind accessors rather
than a public `Vec`, because unique IDs and gapless counter numbering cannot
survive arbitrary external mutation; and shapes distinguish `bounds()` from
`visual_bounds()`, because conflating them makes a shape grow by its own stroke
width on every resize — found by a failing test, which is the only way anyone
ever finds it.

## D30 — The database is a rebuildable cache, not the source of truth

**Decision.** Every capture writes a **durable JSON sidecar** next to a
content-addressed image blob on the filesystem. The SQLite database is an
**index over those files** — a cache that can be deleted, truncated or corrupted
and rebuilt from the sidecars with nothing lost. A file that fails to parse is
**quarantined, never deleted.**

**Why this matters more than it sounds.** D14 promises annotations are never
permanent and D23 promises documents are kept forever. Both promises are only as
strong as the weakest thing they depend on, and a single database is a single
point of failure: one bad `fsync`, one full disk during a write, one power cut,
and a user's entire annotation history is gone. "Kept forever" would then be a
claim the architecture could not honour.

Making the database disposable removes that failure mode entirely rather than
mitigating it. The worst case degrades from *losing everything* to *a slow first
launch while the index rebuilds*.

**Supporting choices that follow from it:**

- **Image blobs live on the filesystem, not in SQLite.** Eviction becomes one
  `unlink`, identical captures deduplicate for free, and the database stays small
  enough to rebuild quickly.
- **`image_hash` is retained after eviction**, with `image_evicted_at` recording
  the loss. An evicted capture still lists, still searches, still opens for
  editing — exactly what D23 requires. The pixels are gone; nothing else is.
- **Document state is a two-variant enum**, so failing to handle the evicted case
  is a compile error rather than a black rectangle at runtime.
- **The annotation document is stored as opaque JSON** and typed on demand, so
  `scrozz-annotate` can keep evolving without invalidating existing history.
- **WAL with `synchronous = FULL`, and `BEGIN IMMEDIATE` for every write**, since
  the GUI and CLI genuinely run concurrently by design (D11).
- **Pinned captures survive even when that makes the size cap unreachable**, and
  the store reports that state rather than silently violating either promise.

## D31 — GNOME/Wayland cannot host our overlays, and we adapt rather than pretend

**The finding.** **Mutter does not implement `wlr-layer-shell`, and this is a
deliberate, stated refusal — not a gap awaiting implementation.** Verified against
mutter `main` at `82ad6279`: there is no layer-shell protocol XML in
`src/wayland/protocol/`, and `src/meson.build` does not generate it. Issue
mutter!973 was closed as a duplicate of gnome-shell!1141, where a GNOME maintainer
states plainly: *"we don't intend to support third party panels, lock screens,
notification UI's etc."* `gtk-layer-shell`'s own README independently lists
GNOME-on-Wayland as unsupported.

**There is no replacement to wait for.** `ext-layer-shell-v1` (MR !28), `xdg-pip`
(!132) and `ext-toplevel-placement-v1` (!389) are all unmerged drafts.

| Compositor | layer-shell | Consequence for Scrozz |
|---|---|---|
| **KWin** (Plasma ≥ 5.20) | ✓ | Overlays work fully |
| **wlroots** (sway, Hyprland) | ✓ | Overlays work fully |
| **Mutter** (GNOME) | **✗ deliberate** | **Overlays cannot be positioned at all** |

**Why this is serious.** Wayland clients cannot set absolute window position —
`xdg_shell` omits it on purpose — so layer-shell is the *only* way to place a
floating surface. Without it, on GNOME/Wayland:

- the **capture stack** (D28) cannot be anchored to the bottom-left;
- the **capture dock** (D20) cannot be anchored anywhere;
- the **selection overlay** cannot cover the screen as a client-drawn surface;
- **pinned captures** cannot be placed.

That is most of the product's surface, on the most common Linux desktop. D8
promises full GNOME support, and taken naively that promise is now unkeepable.

**Decision.** Scrozz does not pretend, and does not degrade silently. Three
responses, in order of preference per surface:

1. **Region selection goes through the portal on GNOME.** The `Screenshot`
   portal's interactive mode hands selection to GNOME Shell's own selector. It is
   not our UI and we cannot theme it, but it is the *correct* mechanism there, and
   it works. GNOME Shell's internal screenshot UI is compositor-owned and its
   D-Bus API is allowlisted to the portal backend, so replicating it from an
   external app is not merely hard — it is closed off.

2. **The capture stack falls back to an ordinary window on GNOME/Wayland.** A
   normal `xdg_toplevel`, placed by the compositor rather than by us. This
   contradicts D27's "fixed position" property, but the alternative is no capture
   stack at all. It must be visibly a deliberate adaptation, not a broken version
   of the macOS behaviour.

3. **XWayland is documented, not defaulted.** Running under XWayland restores
   absolute positioning and makes every overlay work as designed. It costs correct
   fractional scaling and crisp HiDPI. Offer it as a setting for users who want the
   full experience and accept the trade; never force it.

**What we do not do.** We do not ship a GNOME Shell extension as a requirement —
an app that only works after the user installs a separate extension is not an app
that works. And we do not silently misplace overlays and let users conclude Scrozz
is buggy; per D8 the limitation is stated, in the UI, with the reason.

**Honest restatement of D8.** "Full GNOME and KDE support" now means: **KDE gets
the complete Scrozz experience. GNOME gets full capture, recording, annotation,
OCR and history, with compositor-owned region selection and a
compositor-positioned capture stack.** That is a real difference and it belongs in
the comparison table, not buried in a footnote.

## D32 — Releases use Plozz-style calendar versions

**Decision.** Scrozz's user-facing version is the build date in unpadded
`YYYY.M.D` form. A build made on August 27, 2026 is version **`2026.8.27`** and
the corresponding Git tag is **`v2026.8.27`**. Same-day builds share the
marketing version and are distinguished by a separate monotonically increasing
numeric build number.

The initial public builds may still be marked as pre-releases on GitHub while
their maturity warrants it. "Pre-release" is a distribution-channel state, not
part of the version string: there is no `alpha`, `beta` or `rc` suffix in the
app's version.

**Why.** This is the established Plozz convention, needs no manual version
bookkeeping, communicates recency directly, and sorts correctly because every
component is numeric. Keeping maturity out of the version also avoids feeding
Apple an invalid `CFBundleShortVersionString`.

---

# Open questions

- **The Scrozz design language.** Seeded by the spike's token layer; needs
  deliberate definition rather than inheritance from throwaway code. This is the
  first task of the UI crate.

- **Text rendering in annotations.** `tiny-skia` has no text support, so the
  annotation renderer currently ships a built-in single-stroke vector font:
  lowercase renders as small caps, there is no kerning, and unknown glyphs are
  tofu. It buys determinism and headless CI with zero font configuration, which
  is genuinely useful for D25's golden images, but **it is not shippable as the
  user-facing text tool.** Real shaping is needed — `cosmic-text` or `swash`,
  both of which bring font discovery and complex-script support. The constraint
  to preserve when replacing it: whatever is chosen must render *identically* in
  headless CI on all three platforms, or D25's golden images become flaky and get
  disabled.

## Closed since last revision

- ~~**Onboarding and first-run flow**~~ — settled in **D26**.
- ~~**The eframe API-divergence question**~~ — **there was no divergence.** The
  spike concluded the environment shipped a patched eframe because
  `fn ui(&mut self, ui, frame)` did not match the signature it remembered.
  Verified against crates.io and the vendored source: **eframe 0.36.1 is the
  genuine latest published release, from the registry with a checksum, and that
  *is* its real upstream `App` trait.** Nothing is patched, reproducibility for
  outside contributors was never at risk, and there is nothing to resolve.

  The real lesson is a process one, and it generalises past egui: **an agent's
  memory of a fast-moving 0.x crate will be older than the pinned version, and the
  tempting explanation for a mismatch is that the environment is nonstandard.** It
  usually is not. Agents working on Scrozz read the vendored source under
  `~/.cargo/registry/src/` before writing against a pinned dependency, and never
  explain away a compile error by assuming a patched toolchain.

  One genuine supply-chain concern from the spike does stand, and is unrelated:
  **`window-vibrancy` is pinned to an unreleased git revision** for
  `apply_liquid_glass`. Revisit before shipping.
