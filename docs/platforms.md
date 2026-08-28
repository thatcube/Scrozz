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

- Does the capture overlay actually avoid stealing focus while the user types?
- Does `wlr-layer-shell` position the overlay correctly on KDE, and what exactly
  happens on GNOME, which does not implement it?
- Do global hotkeys survive a UAC prompt, a lock screen, a Space switch?
- Do the permission dialogs say something a real person can act on?
- Does drag-and-drop out of the capture stack land correctly in a real app?

These need a real desktop session: Windows 11 on ARM under UTM or Parallels, and
a Linux VM running **both GNOME and KDE**, since the Wayland story differs sharply
between them. This is the slowest and most manual layer, so it is reserved for
behaviour the first three layers structurally cannot reach.

---

## Interactive capture selection

The selector is one state machine with platform-specific hosting. Region,
window, display and all-display modes share the same measured desktop geometry,
HUD and outcome contract. The client-owned route supports drag creation,
move/resize handles, arrow-key nudging, Alt+arrow resizing, Shift for 10-point
steps, exact size, aspect lock, remembered regions, retake, Escape cancellation,
crosshairs, a frozen backdrop and a pixel magnifier. All-in-One exposes the
available modes in the same HUD rather than opening a second picker.

Freeze applies to region and single-display choices, where the pre-overlay
display frame can be returned exactly (and cropped for a region). Window mode
stays live so the backend can preserve the window's native isolation, shape and
shadow; all-display mode stays live so the backend owns mixed-scale composition.

Mixed-DPI desktops stay in logical coordinates while the user selects. A region
is owned and clamped by one measured display, and only that display's scale is
used to round the final rectangle outward to physical pixels. Window outcomes
use the enumerator's owning display rather than the primary display. This avoids
both negative-origin errors and the common 1.0x/1.5x boundary mismatch.

The app reuses its existing eframe loop instead of starting a nested event loop:
the capture worker blocks on the synchronous selector trait while the main
thread hides the capture cards, waits one frame, prepares any frozen pixels,
shows the desktop-sized selector, hides it after commit, captures, then restores
the cards. Only one selector may own that lifecycle at a time. CLI one-shot
selection uses the same bridge in an ordinary temporary window. That one-shot
window intentionally skips reversible AppKit panel conversion, so its layering
across Spaces and fullscreen apps still needs a native smoke run. On X11, both
hosts retain their exact override-redirect window ID, take keyboard ownership
with `SetInputFocus` after the window becomes viewable, and restore the prior
focus before capture begins unless the user has already focused somewhere else.
The selector consumes terminal key, modifier and pointer releases before
restoring focus, so no half of the finishing gesture reaches the prior app.

Shortcut settings and runtime actions are separate names:
`hotkey.capture-region` persists the accelerator, while `capture.region` is the
action registered with the hotkey backend. Registration and tray visibility use
the same live gate: the capture backend must be ready, the session must support
the target, and the selector must report support for the requested mode. An
unavailable selector therefore cannot leave a shortcut that fires an inert
command.

| Session | Planned host | Current selector result |
|---|---|---|
| macOS | Client overlay | Implemented; the long-running window switches between non-activating card and selection behavior |
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

Selector geometry, state, input, accessibility labels, frozen-pixel sampling and
deterministic harness scenes are covered headlessly. The remaining native gaps
are one-shot macOS layering across Spaces/fullscreen apps, mapped layer-shell
rendering, compositor-owned result adaptation, Wayland all-display composition,
portal-provided optional geometry, GNOME/KDE runtime smoke, and hands-on
Windows/X11 focus, DPI and accessibility verification.

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

### Process-global state

4. **`set_event_handler` in both `global-hotkey` and `muda` is a `OnceCell`.**
   Setting it once permanently starves the process-global receiver channel for
   *every* other consumer in the process. Use `receiver()` instead; a library
   crate must never claim the handler.

### Coordinate systems

5. **AppKit is bottom-left origin; Scrozz is top-left.** `NSScreen.frame`,
   `visibleFrame` and Vision's normalised text boxes all need flipping. This is
   the classic "everything is upside down" bug and it is easy to get almost-right.
6. **Windows virtual-desktop coordinates go negative** when a monitor sits left of
   or above the primary. Never assume the origin is `(0, 0)`.

### Scale

7. **There is no single app-wide scale factor on Windows or Wayland.** Windows
   desktops routinely mix 1.0× and 1.5× monitors; Wayland has fractional scaling.
   Scale is per-display, and `ScaleFactor` is `f64` for exactly this reason.

### Testing platform code

8. **`libtest` spawns a thread per test, so no `#[test]` can reach AppKit's main
   thread.** Anything needing the main run loop — `NSApplication`, windows, tray
   items, hotkey managers — is unreachable from an ordinary test. Doctests run on
   the main thread and are the workaround; failing that, test the
   off-main-thread guard and verify real behaviour another way.

---

## Resolved: overlay windows will not steal focus (macOS)

The largest architectural risk in D27 was whether a capture card could be clicked
without pulling focus out of whatever the user was typing in. A plain `NSWindow`
activates its application on click, and eframe/winit creates exactly that.

**It works.** An `NSWindow` converts in place to a non-activating `NSPanel`
(`NSWindowStyleMaskNonactivatingPanel`), verified on this machine against a real
window: `canBecomeKeyWindow == true`, `canBecomeMainWindow == false`. `winit
0.30.13`'s window class declares no ivars, so it is convertible by the same path,
and the conversion is guarded by an instance-size comparison that refuses cleanly
rather than corrupting memory if that ever stops being true.

Key-ness and activation are deliberately separate: the class always answers
`canBecomeKeyWindow` so **Escape still works**, while capture cards additionally
set `becomesKeyOnlyIfNeeded` so a click never takes the user's keystrokes. That
combination is what makes the capture stack usable *while typing* — the normal
case, not an edge case.

---

## Known asymmetry, stated honestly

macOS is where interactive verification happens today, so macOS code will be
better tested than Windows or Linux code until layer 4 exists. That is a real
risk, not a solved problem. Layers 1–3 keep it from becoming *rot* — the code
compiles, runs, and renders on all three — but they do not substitute for someone
using the app on Windows.
