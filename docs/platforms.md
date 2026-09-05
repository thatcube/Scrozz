# Platform strategy

**Scrozz is not a macOS app that will be ported later.** Decision D3 makes the
cross-platform core a property of the first commit, and this document is how that
survives contact with the fact that it is being built on a MacBook.

The problem is real and worth stating plainly: code for a platform you cannot run
is code nobody has ever checked. The mitigation is four layers, each catching a
different class of defect, and each one cheaper than the layer below it.

---

## Layer 1 — Cross-target type checking (local, seconds, free)

`cargo check` does not link, so pure-Rust platform bindings need no foreign SDK.
The Windows target and the non-GTK Linux crates check cleanly from macOS against
the real `windows`, `x11rb`, and `ashpd`/zbus APIs. The complete Linux shell path
still needs target GTK/ATK pkg-config metadata, so it is compiled and tested on
the native Linux CI runner rather than being claimed as a macOS cross-check.

```bash
tools/dev.sh platforms
```

The developer wrapper leases one of a fixed number of Cargo target roots. Cargo
already isolates explicit triples inside that root, so this check does not create
another target directory per platform or per worktree.

This is the layer that changes the character of the work. Windows and Linux code
is compiled against the genuine API surface, so a misremembered method name, a
wrong argument type, a missing feature flag, or a bad trait bound is a **compile
error on this machine** rather than a surprise days later. It is the difference
between writing platform code blind and writing it with the type checker
watching.

### Native build-script boundaries

**Crates whose dependencies compile C cannot be cross-checked without a cross C
toolchain.** `cargo check` still runs build scripts, and `rusqlite`'s `bundled`
feature compiles `sqlite3.c` *for the target*, which fails on this machine with
`fatal error: 'stdlib.h' file not found`. `scrozz-store` and the `scrozz` binary
that depends on it are therefore excluded, via `SCROZZ_XCHECK_EXCLUDE`.

`scrozz-store` is platform-agnostic pure Rust; cross-checking it would prove
almost nothing, and CI compiles it natively on all three runners anyway (layer
2), which is a strictly stronger check.

Linux desktop integration has a second build-script boundary. GTK/ATK discovery
uses pkg-config, and a macOS host has no Linux target sysroot containing that
metadata. `scrozz-capture`, `scrozz-record`, `scrozz-ocr`, `scrozz-ui`, and the
shared core remain cross-checked for Linux; the GTK-backed `scrozz-shell` path is
checked in full on native Linux CI. The helper identifies this failure mode and
prints the narrower command instead of implying that missing metadata is a Rust
compile error.

Recorded-media preview is intentionally asymmetric. macOS uses `AVAssetReader`
for bounded RGBA decode, `AVPlayer` as the authoritative media clock and audio
renderer, and a MediaToolbox audio tap so preview and export share gain, mute,
and channel edits. Windows and Linux compile the same playback contract but
report an explicit unsupported capability until their native decode and audio
adapters are complete; they never substitute a synthetic clock or static source
thumbnail for playback.

Camera capture follows the same native-adapter rule. macOS uses AVFoundation,
Windows uses a Media Foundation source reader, and Linux's opt-in native recorder
uses V4L2 read or mmap streaming I/O. All three feed the same bounded
freshest-frame queue and the same deterministic PiP/presenter compositor; camera
timestamps are mapped onto each recorder's pause-free media clock before
composition, so a live picture-in-picture to presenter switch never moves the
recording clock. Device enumeration is passive: it names devices and their state
without opening one. Permission prompts and camera activation happen only after
an explicit Preview or Record action, and every stop, pause, revocation and drop
path releases the native device and clears the in-app privacy indicator with it.

Windows applies `WDA_EXCLUDEFROMCAPTURE` to Scrozz's own non-activating overlay
through `WindowsOverlay`, so the live camera controls stay on screen without
entering the video. X11 and current Wayland capture paths cannot promise
per-window exclusion; there Scrozz suppresses its overlay chrome while
recording, and the camera's own activity stays visible through the system
privacy indicator rather than through a Scrozz surface that would be recorded.

Two Linux camera limits are refusals, not gaps to be papered over:

* **Self-capture recursion.** Because X11 and Wayland cannot exclude a single
  window from a display capture, a camera preview shown over a running display
  recording would record itself, once per frame, forever. Scrozz refuses to open
  a preview while a recording owns the camera on every platform, and says so,
  rather than compositing a picture of its own preview into the video.
* **MJPEG-only cameras.** The V4L2 adapter negotiates packed `YUYV` and nothing
  else. A camera that can only emit MJPEG is reported as
  `Error::Unsupported { what: "V4L2 camera pixel format", .. }` naming the device
  path, because a silent in-process JPEG decode per frame would be a latency and
  dependency cost the recorder has not agreed to pay.

Completed recordings and editor exports cross into the aggregate UI through
`scrozz_record::handoff::FinalizedMediaHandoff`. The handoff carries durable
ownership, a canonical media path, the exact content type and codec, a bounded
poster with explicit color metadata, duration, dimensions, file size, audio
presence, and media-appropriate actions. GIF is `image/gif`, hardware H.264 is
`video/mp4`, and software AV1 is `video/webm`; no consumer has to infer a
container from a generic "video" bit, and only a real video opens the editor
while a GIF opens the file. Card geometry remains owned by the modern adaptive
Recent Captures Overlay.

Editor export capabilities are asymmetric for the same reason playback is.
macOS probes its hardware-only H.264 writer once per session, with the result
cached, so the editor knows whether MP4 can actually be produced before the
user commits to it. Windows and Linux advertise that path as unavailable until
their native writer adapters exist. The pure-Rust `rav1e`/WebM path is offered
only when the `rav1e-fallback` feature is compiled, and GIF is always
available because it is pure Rust with no platform dependency. A requested MP4
is never silently turned into a WebM: an unavailable codec is reported as an
unavailable codec.

Keeping `bundled` is deliberate: it means shipped builds carry no system SQLite
dependency, which matters far more than local cross-check coverage of a crate
that has no platform code in it.

Its limit is exact: this layer proves the code is *well-formed*, never that it
*works*.

## Layer 2 — Real builds and tests in CI (every push, free)

GitHub Actions runs macOS, Windows and Linux runners, free for public
repositories. Every push compiles and runs the test suite on all three. This
catches what type checking cannot: linking, runtime panics, logic that is wrong
in a platform-specific way, and dependency problems that only appear on a real
sysroot.

## Layer 3 — Headless golden images per platform (every push, free)

This is the layer most projects do not have, and it is the reason decision D25
matters more than it first appears.

Because the screenshot harness renders **headlessly with no display server**, it
runs on the Windows and Linux CI runners exactly as it runs locally. So the UI is
not merely compiled on those platforms — **it is rendered, and the pixels are
diffed against committed baselines.**

That is genuine visual verification on machines nobody owns. Font metrics differ
across platforms, DPI handling differs, text antialiasing differs; all of it shows
up as a pixel diff with a side-by-side artifact attached to the CI run. Without
this, "does the UI look right on Windows" is a question only a human with a
Windows machine can answer, and it therefore goes unanswered.

## Layer 4 — Virtual machines, for what only a real session can show

Some things are behavioural and cannot be faked:

- Does the Recent Captures Overlay actually avoid stealing focus while the user types?
- Does `wlr-layer-shell` position the overlay correctly on KDE, and what exactly
  happens on GNOME, which does not implement it?
- Do global hotkeys survive a UAC prompt, a lock screen, a Space switch?
- Do the permission dialogs say something a real person can act on?
- Does drag-and-drop out of the Recent Captures Overlay land correctly in a real app?

These need a real desktop session: Windows 11 on ARM under UTM or Parallels, and
a Linux VM running **both GNOME and KDE**, since the Wayland story differs sharply
between them. This is the slowest and most manual layer, so it is reserved for
behaviour the first three layers structurally cannot reach.

---

## Interactive capture selection

The selector is one state machine with platform-specific hosting. Region,
window, display and all-display modes share the same measured desktop geometry,
HUD and outcome contract. The client-owned route supports drag creation,
move/resize handles in All-in-One, arrow-key nudging, Alt+arrow resizing, exact
size, aspect lock, remembered regions, retake, Escape cancellation, crosshairs,
a frozen backdrop and a pixel magnifier. Direct Capture Area is intentionally
one gesture: its launcher click is drained, the next press-drag-release captures
immediately, and a no-movement click cancels. Space moves an active rectangle,
Shift constrains creation or movement to one axis while held, and Option/Alt
grows from center. All-in-One exposes the available modes in the same HUD rather
than opening a second picker.

**Settings → Screenshots → Freeze screen when taking a screenshot** is off by
default. When enabled it applies to region and single-display choices, where the
pre-overlay display frame can be returned exactly (and cropped for a region).
Window mode stays live so the backend can preserve the window's native
isolation, shape and shadow; all-display mode stays live so the backend owns
mixed-scale composition. An explicit CLI `--freeze` or `--freeze=false`
overrides the saved preference for that capture.
Frozen frames and thumbnails are presentation data only: they are never inserted
into the window target list and cannot become a desktop-sized capture candidate.
The long-running root window is constructed hidden and parked off-screen because
eframe orders a root window in once after its first rendered frame even when its
builder requested `visible: false`. After that bootstrap pass, idle means a real
AppKit `orderOut:`, not alpha zero or click-through. Capture-card mode sizes the
native root to the occupied stack column plus the complete gesture envelope,
expanding only as card count grows and freezing its origin during an in-progress
drag. An ordinary Settings/editor child keeps only a one-point off-screen parent
bootstrap alive; the parent is not on any display. All card/selector roots and
secondary selector viewports use public content-protection APIs
(`NSWindowSharingNone` on macOS), while ordinary Settings/editor viewports keep
the default externally capturable sharing mode.

ScreenCaptureKit may still enumerate the identity and bounds of a protected
window; AppKit's sharing type prevents its pixels from being read, not its
metadata from being listed. That is why card mode is content-bounded rather than
leaving a protected but desktop-sized blank candidate. External pickers are
expected to honor the public protection flag; a picker that deliberately offers
protected utility chrome may still show its compact bounds but cannot capture
its contents.

An empty/parked/selector root never transitions directly to visible card mode.
The host first orders it out, applies the authoritative card geometry for the
owning display, and waits for two consecutive native viewport observations to
match position, logical size, and native pixels-per-point. Only then does it
install non-click-through card input/tracking and order the root in. Eframe may
paint the incoming card while the native root is still ordered out; either way,
the first on-screen frame comes from the final framebuffer rather than the
parked 1×1/1× bootstrap. Card-count changes, display moves/scale changes, and
returns from selection use the same hidden arming barrier; every empty-to-first
cycle starts a new barrier. Platforms that do not expose window geometry
(Wayland) use two hidden event-loop turns, and start at final card geometry
rather than macOS's 1×1 parked frame. Overlay contexts lock egui zoom to 1× and
disable keyboard zoom: their geometry already uses native OS points, so a second
application-level scale would desynchronize viewport commands, hit testing, and
the readiness barrier.

Window selection has a stricter invocation boundary than the other modes. The
selector takes a fresh native target snapshot before it sends any event that can
raise, resize, show or create a Scrozz picker surface. That snapshot is consumed
once and never cached for a later command. On macOS, ScreenCaptureKit supplies
capturable identity, frame and visibility while CoreGraphics supplies the
current Space's exact front-to-back on-screen order; the two are joined by
`CGWindowID`. Scrozz's own process is excluded before the snapshot reaches the
UI. Pointer hit testing preserves that order and chooses the first visible
eligible frame containing the pointer, so an underlying window is reachable only
through an exposed portion and a fully covered window is not reachable. The
Finder desktop, when ScreenCaptureKit exposes it as a normal window, remains
behind ranked application windows and is reachable only on exposed desktop.
Pointer interaction is resolved after the HUD is laid out so controls can exclude
the canvas underneath them. When that changes a window/display target, the
selector compares a monotonic highlight revision with the revision just painted
and discards exactly that stale pass. Egui immediately paints one replacement
pass in the same rendered frame; movement within the same target advances no
revision and schedules no replacement. This avoids relying on a later event-loop
wake after a native menu closes, without turning the picker into a continuous
repaint loop.

Mixed-DPI desktops stay in logical coordinates while the user selects. A region
wholly contained by one display retains that display's measured scale for exact
outward pixel rounding. A region spanning displays carries no false single-
display ownership and is handed to the backend's virtual-desktop capture path.
Window outcomes use the enumerator's owning display rather than the primary
display. This avoids both negative-origin errors and the common 1.0x/1.5x
boundary mismatch.

The app reuses its existing eframe loop instead of starting a nested event loop:
the capture worker blocks on the synchronous selector trait, snapshots native
targets, and only then asks the main thread to hide capture cards for an
interactive mode. After a card-free frame it prepares presentation pixels, waits
for launcher input to become quiescent, shows the desktop-sized selector, hides
it after commit, captures, then restores the cards to their prior slots; only the
new card animates. Immediate fullscreen capture leaves cards visible when the
backend can exclude Scrozz from the output. Only one selector may own that
lifecycle at a time. CLI one-shot
selection uses the same bridge in an ordinary temporary window. All stable-winit
macOS viewports preserve their original runtime class and delegate. On X11, both
hosts retain their exact override-redirect window ID, take keyboard ownership
with `SetInputFocus` after the window becomes viewable, and restore the prior
focus before capture begins unless the user has already focused somewhere else.
The selector consumes terminal key, modifier and pointer releases before
restoring focus, so no half of the finishing gesture reaches the prior app.
Escape releases pointer input in the handling pass. The transparent selector
retains terminal-key ownership only until Escape key-up, then restores the arrow,
hides, releases native focus, completes the cancellation handshake, and frees the
single-selection gate. Menu and global-hotkey routes share this state machine and
can begin another invocation immediately after restoration.
AppKit order-in/order-out animation is disabled for this utility root.
Without that, `isVisible` becomes false before the fade completes while
CoreGraphics still reports the old fullscreen window; terminal outcomes now
leave native enumeration on the next bounded window-server turn.

Shortcut settings and runtime actions are separate names:
`hotkey.capture-region` persists the accelerator, while `capture.region` is the
action registered with the hotkey backend. Registration and tray visibility use
the same live gate: the capture backend must be ready, the session must support
the target, and the selector must report support for the requested mode. An
unavailable selector therefore cannot leave a shortcut that fires an inert
command.

| Session | Planned host | Current selector result |
|---|---|---|
| macOS | Client overlay | Implemented with winit-owned ordinary windows; non-activating panel behavior is deferred until stable winit exposes native panel construction through eframe |
| Windows | Client overlay | Implemented and type-checked; native focus, z-order and mixed-DPI behavior still need a real Windows session |
| Linux/X11 | Client overlay | Implemented and type-checked, including direct focus ownership/restoration; native keyboard behavior still needs an X11 smoke run |
| KDE/wlroots Wayland | Layer shell | Unavailable: layer-shell may be advertised, but Scrozz does not yet own a mapped layer-shell rendering surface |
| GNOME/Mutter Wayland | Compositor-owned | Unavailable through `RegionSelector`: the Screenshot portal returns an image URI, not target geometry |
| Headless | None | Refused with `Error::Unsupported` |

Wayland has two intentionally separate boundaries:

- ScreenCast/PipeWire owns capture permission and may let the compositor choose
  a window, but it does not reveal a reusable `WindowId` or desktop rectangle.
  That portal-owned capture cannot be represented by inventing a selector
  outcome.
- The Screenshot portal is not implemented in Scrozz. Even when its interactive
  UI is present, its image URI is not selection geometry and is never treated as
  such.

Accordingly, `--interactive window` is not advertised as a Wayland workaround.
Window enumeration and the selector both refuse truthfully until capture has a
targetless portal-owned handoff. Wayland region cropping likewise requires real
portal stream position and size; missing geometry is an error, never guessed.
All-display composition remains separate capture-backend work.

Native privacy remains authoritative. macOS Screen Recording permission can
redact titles or refuse enumeration until granted, and protected/DRM windows may
be omitted, return blank pixels, or fail capture even when their frame is visible.
Windows can similarly withhold metadata for higher-integrity or protected
processes. Scrozz does not bypass those controls or substitute pixels from a
different target. Minimized, off-screen, and other-Space windows are never
pointer-selectable; a window closing or moving Spaces after the snapshot is
reported as a gone target instead of silently selecting another window.

Selector geometry, state, input, accessibility labels, frozen-pixel sampling and
deterministic harness scenes are covered headlessly. The remaining native gaps
are one-shot macOS layering across Spaces/fullscreen apps, mapped layer-shell
rendering, compositor-owned result adaptation, Wayland all-display composition,
portal-provided optional geometry, GNOME/KDE runtime smoke, and hands-on
Windows/X11 focus, DPI and accessibility verification.

---

## Scrolling capture

Scrolling capture is the one still-capture path that needs *input*, not just
pixels, so its platform story is separate from the selector's.

Still captures use the ordinary `CaptureBackend` API. Scrolling capture opens
`scrozz_capture::frame_session` instead: one target grant is retained across
every viewport frame rather than re-acquired per frame, and the session is torn
down in reverse order when dropped. Every `capture_frame` returns the newest
complete observation that is newer than the previously delivered one, so a frame
buffered while the page was still settling is accepted rather than flushed.

The app does not guess a scrolling window. The user first draws the exact area
inside one visible application window, then chooses **Manual** or **Automatic**
and presses **Start capture**. The first coherent viewport displacement selects
up, down, left, or right from the captured pixels—there is no axis or direction
picker to get wrong. Manual mode follows that route while the user scrolls.
Automatic mode waits for that first user scroll, then continues in the same
direction with bounded native wheel steps. Both modes save only after **Finish**
and remain discardable until then. Stationary frames do not consume the capture
budget; Automatic pauses its wheel input until movement resumes. Lost overlap
waits for the user to scroll back and reconnect, without saving a partial image.
Capture limits or later acquisition failures hold the valid pixels in memory
and keep **Finish** and **Discard** available. Reverse routes are normalized before
append-only stitching and flipped back for a naturally ordered final image. A
failed automatic attempt keeps setup visible so the same area can be retried
manually.

Input delivery is per-platform and never global:

* **macOS** synthesizes small, line-based target-bound wheel gestures. The selected window is
  resolved against CoreGraphics' documented front-to-back list before a gesture
  is posted, so a window that moved behind another after selection is an error
  rather than a scroll delivered to whatever is now on top. One retained
  ScreenCaptureKit filter/configuration supplies every frame, and its own
  point-to-pixel scale is carried into frame metadata.
* **Windows** does not use global `SendInput`: wheel input normally follows
  keyboard focus, which could scroll a terminal or another window after a focus
  change. Scrozz snapshots the selected `HWND` together with its process creation
  time, UI thread and class, revalidates that identity before every gesture,
  resolves the child at the selected point, and sends one conservative
  `WM_MOUSEWHEEL` or `WM_MOUSEHWHEEL` detent directly with a timeout. A recycled
  `HWND`, moved target, hung process or UIPI rejection is an error, never
  reported as successful scrolling. The selected `DisplayId` is retained through
  the gesture so overlapping logical rectangles on mixed-DPI desktops are not
  re-resolved heuristically.
* **X11** keeps automatic XTEST scrolling, pointer-warped to the target and
  restored afterwards.
* **Wayland scrolling input is deliberately manual.** A separate RemoteDesktop
  portal grant cannot guarantee that synthesized wheel input reaches the same
  surface the user selected in ScreenCast, so Scrozz never prompts for a grant it
  cannot bind safely. There, `--scrolling` and `--scrolling=active` mean "choose
  one window in the ScreenCast portal"; explicit `primary` or display-ID
  selectors are rejected before the picker opens.

An automatic capture never posts input until the overlay's own window reports
back that it has actually become mouse-transparent. The queued egui viewport
command is not evidence: on a platform with no native click-through readback the
capture stays manual rather than scrolling the overlay instead of the page.

Cancellation is always answerable, and a cancelled or stalled session still
salvages what it aligned: the partial stitch is written through the same atomic,
never-overwriting output path as a completed one.

---

## What this means for anyone writing platform code

1. **Never guess an API.** Read the vendored bindings under
   `~/.cargo/registry/src/` before writing the call. These crates are generated,
   they move fast, and your memory of them is probably older than the pinned
   version.
2. **Cross-check before committing.** `tools/dev.sh platforms` is fast, and
   a Windows or Rust-only Linux compile error found here costs a minute instead
   of a CI round trip. Native Linux CI owns GTK/ATK-backed shell validation.
3. **Keep `cfg(target_os)` out of the crates above the platform layer.** Only
   `scrozz-capture`, `scrozz-record`, `scrozz-ocr` and `scrozz-shell` may contain
   it. Everything else is platform-agnostic by construction, which is what keeps
   the majority of the codebase verifiable everywhere at once.
4. **Write the platform gaps down rather than hiding them.** Per decision D8,
   `Error::Unsupported` with a truthful `why` is the correct outcome when a
   compositor genuinely cannot do something. A gap that is documented is a
   limitation; a gap that is papered over is a bug report.

## Platform gotchas discovered while building

Recorded as they are found, because each one cost real time and every one of
them is invisible until it bites.

### Silent failures — APIs that lie about succeeding

These are the dangerous class: the call returns success and nothing works.

1. **macOS `RegisterEventHotKey` returns `noErr` for shortcuts the system already
   owns** (`Cmd+Shift+4`, for instance). Registration "succeeds", the handler
   never fires, and no API will tell you. **Never trust its return value** —
   conflicts must be caught up-front against a table of reserved shortcuts.
2. **`global-hotkey` is X11-only on Linux.** On Wayland it falls back to a no-op
   that returns `Ok(())`. Taken at face value, Scrozz would report hotkeys as
   working on every Wayland session while none of them fire. Detect the session
   and return `Error::Unsupported` per D11 rather than believing the crate.
3. **`tray-icon` and `muda` types are `Rc`-based and `!Send`**, and — along with
   `GlobalHotKeyManager` — require the main thread with a live event loop.
   Failure to satisfy that is also silent.
4. **PipeWire delivers empty buffers, and they look exactly like real ones.**
   Mutter hands over a buffer with `chunk->size == 0` when nothing on screen has
   changed. Accepting the first buffer offered therefore yields a black PNG on
   an idle desktop — a structurally perfect frame containing nothing. A still
   capture must keep waiting until a buffer actually carries pixels.
5. **A malformed SPA POD is not an error, it is a hang.** The server does not
   reject a parameter it cannot read; the stream simply never reaches
   `Streaming`. Encoding bugs must be caught by byte-level tests, because at
   runtime they present as a timeout with nothing to go on.
6. **`spa_pod_builder_pad` does nothing inside a Choice or an Array.** Every POD
   is padded to eight bytes *except* the alternatives inside a choice body,
   which are packed contiguously at exactly the child's size. Padding them "for
   consistency" produces a POD the server reads as garbage — see the previous
   entry for how that presents.

7. **A full Linux workspace check from macOS reaches target `pkg-config`.**
   `scrozz-shell -> tray-icon -> libappindicator -> GTK` asks for Linux
   GLib/GObject/GIO/Pango/GTK metadata even though `cargo check` never links.
   Without a Linux sysroot that failure is expected; check `scrozz-capture`
   independently and let the Ubuntu CI gate check the native shell. Never point
   `PKG_CONFIG_ALLOW_CROSS` at the host's Darwin libraries.

### Process-global state

8. **`set_event_handler` in both `global-hotkey` and `muda` is a `OnceCell`.**
   Setting it once permanently starves the process-global receiver channel for
   *every* other consumer in the process. Use `receiver()` instead; a library
   crate must never claim the handler.

### Coordinate systems

9. **AppKit is bottom-left origin; Scrozz is top-left.** `NSScreen.frame`,
   `visibleFrame` and Vision's normalised text boxes all need flipping. This is
   the classic "everything is upside down" bug and it is easy to get almost-right.
10. **Windows virtual-desktop coordinates go negative** when a monitor sits left of
   or above the primary. Never assume the origin is `(0, 0)`.
11. **A PipeWire stride is not `width * 4`.** The producer picks it, and it is
    routinely larger. Reading the buffer linearly gives the classic diagonal
    shear, which looks like a decoding bug and is not. The last row is also
    entitled to end after `width * 4` bytes rather than a full stride, so
    demanding one more byte rejects a perfectly good buffer.

### Scale

12. **There is no single app-wide scale factor on Windows or Wayland.** Windows
    desktops routinely mix 1.0× and 1.5× monitors; Wayland has fractional scaling.
    Scale is per-display, and `ScaleFactor` is `f64` for exactly this reason.
13. **On Wayland, do not use a display's scale factor to convert a region to
    pixels.** Under fractional scaling the compositor rounds the output's pixel
    size, so the nominal scale and the real ratio disagree. The delivered frame
    and the portal's reported logical size describe the same monitor, so their
    ratio is the only figure that is a fact rather than an assumption.

### Testing platform code

14. **`libtest` spawns a thread per test, so no `#[test]` can reach AppKit's main
    thread.** Anything needing the main run loop — `NSApplication`, windows, tray
    items, hotkey managers — is unreachable from an ordinary test. Doctests run on
    the main thread and are the workaround; failing that, test the
    off-main-thread guard and verify real behaviour another way.
15. **A skipped test that exits 0 is worse than no test.** It is
    indistinguishable from a pass, so it records verification that never
    happened. `tools/wayland-smoke.sh` exits 77 with the reason on stderr
    instead.

---

## Safety boundary: winit owns macOS window identity

Winit 0.30.13 owns each `WinitWindow`, its delegate, and KVO registrations such
as `effectiveAppearance`. AppKit can add further observers after eframe's app
creator returns. Runtime `object_setClass` conversion to an `NSPanel` therefore
cannot be made lifecycle-safe: it disconnects whichever KVO subclass is current,
and a later observer removal raises an uncaught `NSRangeException` during
teardown.

Scrozz never changes the class or delegate of a winit-owned window. The macOS
adapter retains the window only while applying documented level, collection,
opacity, sharing, cursor, and order-in/order-out properties, then drops that
retain before eframe destroys its viewport map. This temporarily leaves capture
cards as ordinary floating windows that may activate Scrozz when clicked.

Winit's native `NSPanel` construction landed in
[rust-windowing/winit#4035](https://github.com/rust-windowing/winit/pull/4035)
for the 0.31 line. Scrozz can restore non-activating behavior after both winit
0.31 and an eframe release exposing its macOS panel attribute are stable; no
unrelated KVO or IME patch is vendored in the meantime.

---

## Pinned-window contracts and limits

Pin geometry always comes from a native monitor enumerator. If that query fails,
Pin to Screen is disabled with the platform error; the app never substitutes a
made-up `1440x875 @ 1x` desktop. A pin viewport is also bounded to a 4096-pixel
edge and 8,388,608 physical pixels after applying the destination monitor's
scale. Moving from a 1x to a 4x display can therefore reduce the logical pin size
before the next backing surface is allocated. That cannot trap the pin. Pins
keep a directly clickable Close control at every recoverable size, add Lock,
scale, and movement controls as room becomes available, and resize from all four
edges and corners while preserving the source aspect ratio. Secondary-clicking
an unlocked image opens a separate action window, so a tiny pin cannot clip its
own menu; it includes durable Copy, Save As, Upload, Extract Text, annotation,
lock, size, opacity, chrome, Close, and Close All actions. Those content actions
resolve the persisted capture identity rather than depending on a live Recent
Captures card.

A locked image remains click-through, but its Lock and Close controls are
selective interactive islands. Hovering the locked image dims only the image to
make that state legible; the two controls remain fully opaque. If a platform
cannot establish pointer passthrough or an external lock escape, Scrozz refuses
the lock rather than creating a trapping window.

| Session | Non-activation and stacking | Placement, desktops, and lock limits |
|---|---|---|
| macOS | Each child viewport retains its original winit-owned class and delegate. Non-activation is reported unavailable until eframe can request winit's native `NSPanel`; ordinary floating windows are used instead. | Native global geometry, opacity, click-through, all Spaces, and fullscreen auxiliary behavior are applied without runtime class mutation. |
| Windows | The process-owned HWND is verified by PID plus exact title, then receives `WS_EX_NOACTIVATE`, `WS_EX_TOOLWINDOW`, `WS_EX_LAYERED`, `HWND_TOPMOST`, and a `WM_MOUSEACTIVATE -> MA_NOACTIVATE` subclass. Ambiguous lookup fails closed. | Negative virtual-desktop coordinates work. Explicit placement is disabled on mixed-DPI desktops until the Win32 topology model has one coherent global logical mapping. Windows exposes no supported API to place an ordinary app window on every virtual desktop, so pins stay on their current desktop. A no-activate pin does not promise keyboard nudges while another app owns focus. |
| X11 | Scrozz requests a managed Dock window, ICCCM `input = false`, removes `WM_TAKE_FOCUS`, and asks for Above, Sticky, SkipTaskbar, and SkipPager. These are window-manager policy hints, not a portable focus guarantee, so the UI says so and lock remains disabled. | The shared X11 coordinate space and detected server scale are used. A WM may ignore placement, stacking, stickiness, or focus hints. Override-redirect is deliberately not used for movable pins because it breaks WM move/resize behavior. |
| Wayland | An ordinary `xdg_toplevel` cannot promise non-activation or an always-on-top layer. Scrozz does not infer layer-shell availability from a compositor name and does not claim support until it has actually bound the advertised protocol. | `xdg-shell` has no global positioning. wlroots/KWin compositors may offer layer-shell; GNOME/Mutter does not. Until a native adapter exists, compositor window rules are the honest workaround. XWayland is an explicit crispness/fractional-scaling trade-off, never an automatic fallback. |

A capture reaches a pin from every source that can name a target. The fullscreen
hotkey and tray entry go straight through the pipeline; a `scrozz capture` typed
at a terminal while the app is running is executed *inside* it over a Unix
socket or a current-user-only Windows named pipe. Its pixels are moved into the
same capture stack before the caller receives success, so they receive history
identity, a bounded texture, and Pin to Screen exactly as a hotkey capture does.
The handoff admits one frame per request and at most two queued full-resolution
frames; additional burst requests receive an explicit busy error instead of
growing memory without limit. Choosing a region or a window *on screen* runs the
selection overlay first and then rejoins that same handoff, so an interactively
chosen target is bounded and identified identically to a named one.

Static pins do not drive a repaint clock. Capture completion, IPC, menu/hotkey
input, viewport interaction, animation, geometry settlement, and explicit
content changes wake the event loop. Native pin properties are delta-applied, so
an unrelated wake produces no repeated window mutation.

---

## Known asymmetry, stated honestly

macOS is where interactive verification happens today, so macOS code will be
better tested than Windows or Linux code until layer 4 exists. That is a real
risk, not a solved problem. Layers 1–3 keep it from becoming *rot* — the code
compiles, runs, and renders on all three — but they do not substitute for someone
using the app on Windows.
## Native credential adapters

Cloud-enabled packages select the operating system's credential service:
macOS Keychain, Windows Credential Manager, and freedesktop Secret Service on
Linux. CI's native three-platform matrix builds and tests `--all-features`, so
each adapter is compiled against its real target API. Runtime availability still
depends on a logged-in keychain/session service; Settings reports that state
truthfully. A real write/read/delete smoke is opt-in with
`SCROZZ_TEST_NATIVE_VAULT=1` and must run only in an unlocked interactive
desktop session.
