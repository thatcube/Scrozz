# Platform strategy

**Scrozz is not a macOS app that will be ported later.** Decision D3 makes the
cross-platform core a property of the first commit, and this document is how that
survives contact with the fact that it is being built on a MacBook.

The problem is real and worth stating plainly: code for a platform you cannot run
is code nobody has ever checked. The mitigation is four layers, each catching a
different class of defect, and each one cheaper than the layer below it.

---

## Layer 1 — Cross-target type checking (local, seconds, free)

`cargo check` does not link, so it needs no Windows SDK and no Linux sysroot.
Both targets check cleanly from macOS today for **every crate that contains
platform-specific code** — including the real platform bindings: the `windows`
crate, `x11rb`, and `ashpd` with its zbus stack.

```bash
tools/check-all-platforms.sh
```

This is the layer that changes the character of the work. Windows and Linux code
is compiled against the genuine API surface, so a misremembered method name, a
wrong argument type, a missing feature flag, or a bad trait bound is a **compile
error on this machine** rather than a surprise days later. It is the difference
between writing platform code blind and writing it with the type checker
watching.

### The one exception, and why it does not matter

**Crates whose dependencies compile C cannot be cross-checked without a cross C
toolchain.** `cargo check` still runs build scripts, and `rusqlite`'s `bundled`
feature compiles `sqlite3.c` *for the target*, which fails on this machine with
`fatal error: 'stdlib.h' file not found`. `scrozz-store` and the `scrozz` binary
that depends on it are therefore excluded, via `SCROZZ_XCHECK_EXCLUDE`.

This costs nothing real. Only four crates are permitted to contain
`cfg(target_os)` at all — `scrozz-capture`, `scrozz-record`, `scrozz-ocr` and
`scrozz-shell` — and **all four are still fully checked on all three targets**.
`scrozz-store` is platform-agnostic pure Rust; cross-checking it would prove
almost nothing, and CI compiles it natively on all three runners anyway (layer
2), which is a strictly stronger check.

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

## What this means for anyone writing platform code

1. **Never guess an API.** Read the vendored bindings under
   `~/.cargo/registry/src/` before writing the call. These crates are generated,
   they move fast, and your memory of them is probably older than the pinned
   version.
2. **Cross-check before committing.** `tools/check-all-platforms.sh` is fast, and
   a Windows compile error found here costs a minute instead of a CI round trip.
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

### Alpha

8. **Un-premultiplying a fully transparent pixel produces black**, because
   `c / 0` has no other honest answer. Box-filtering those straight channels
   alongside an opaque neighbour drags that black into the result and rings a
   window capture in a dark fringe — D9 acceptance criterion 3, failed. Every
   minification of capture pixels therefore weights each sample by its own
   alpha, so a transparent pixel contributes its transparency and nothing else.
   Both box filters — `Thumbnail::downscale` and `scrozz_ui::overlay_app` — do
   this, and both tile the source with floor/floor spans so the outermost row
   and column are weighted like every other one rather than shared with a
   neighbour.

### Testing platform code

9. **`libtest` spawns a thread per test, so no `#[test]` can reach AppKit's main
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

## Pinned-window contracts and limits

Pin geometry always comes from a native monitor enumerator. If that query fails,
Pin to Screen is disabled with the platform error; the app never substitutes a
made-up `1440x875 @ 1x` desktop. A pin viewport is also bounded to a 4096-pixel
edge and 8,388,608 physical pixels after applying the destination monitor's
scale. Moving from a 1x to a 4x display can therefore reduce the logical pin size
before the next backing surface is allocated.

Enlarging a pin needs a real display density, because the bound is in *physical*
pixels. When a pin's frame lands on no display the process can name — a monitor
unplugged between two frames, a topology query that has not settled — the
strictest density still on the desk is used instead, and a desk with no displays
at all caps growth at the size already allocated. Shrinking is always permitted;
it can only release memory.

A pin's on-screen lifetime is owned by the overlay, not by arrival order. A
restore issued by the host can be in flight while the user closes the same pin,
and the host learns of the close only afterwards; the overlay records identities
it has retired and refuses to reopen one, so no ordering can make a dismissed
pin reappear. A deliberate new pin revives the identity.

| Session | Non-activation and stacking | Placement, desktops, and lock limits |
|---|---|---|
| macOS | Each child viewport is adopted as the same non-activating `NSPanel` used by the capture stack. Adoption is runtime-checked; a failed conversion is shown inside that pin rather than reported as success. | Native global geometry, opacity, click-through, all Spaces, and fullscreen auxiliary behavior are applied. |
| Windows | The process-owned HWND is verified by PID plus exact title, then receives `WS_EX_NOACTIVATE`, `WS_EX_TOOLWINDOW`, `WS_EX_LAYERED`, `HWND_TOPMOST`, and a `WM_MOUSEACTIVATE -> MA_NOACTIVATE` subclass. Ambiguous lookup fails closed. | Negative virtual-desktop coordinates work. Explicit placement is disabled on mixed-DPI desktops until the Win32 topology model has one coherent global logical mapping. Windows exposes no supported API to place an ordinary app window on every virtual desktop, so pins stay on their current desktop. A no-activate pin does not promise keyboard nudges while another app owns focus. |
| X11 | Scrozz requests a managed Dock window, ICCCM `input = false`, removes `WM_TAKE_FOCUS`, sets `_NET_WM_USER_TIME` to 0 — EWMH's own "this window was not asked for by user interaction, do not focus it" — and asks for Above, Sticky, SkipTaskbar, and SkipPager. These are window-manager policy hints, not a portable focus guarantee, so the UI says so and lock remains disabled. Window opacity is a compositing-manager feature and is not claimed; the composited image-opacity fallback is used instead. | The shared X11 coordinate space and detected server scale are used. A WM may ignore placement, stacking, stickiness, or focus hints. Override-redirect is deliberately not used for movable pins because it breaks WM move/resize behavior. |
| Wayland | An ordinary `xdg_toplevel` cannot promise non-activation or an always-on-top layer. Scrozz does not infer layer-shell availability from a compositor name and does not claim support until it has actually bound the advertised protocol. | `xdg-shell` has no global positioning. wlroots/KWin compositors may offer layer-shell; GNOME/Mutter does not. Until a native adapter exists, compositor window rules are the honest workaround. XWayland is an explicit crispness/fractional-scaling trade-off, never an automatic fallback. |

A capture reaches a pin from every source that can name a target. The fullscreen
hotkey and tray entry go straight through the pipeline; a `scrozz capture` typed
at a terminal while the app is running is executed *inside* it over a Unix
socket or a current-user-only Windows named pipe. Its pixels are moved into the
same capture stack before the caller receives success, so they receive history
identity, a bounded texture, and Pin to Screen exactly as a hotkey capture does.
The handoff admits one frame per request and at most two queued full-resolution
frames; additional burst requests receive an explicit busy error instead of
growing memory without limit. Choosing a region or a window *on screen* still
needs the selection overlay, which does not exist yet, so that one path refuses
per D8 and names `--region` / `--window` as the route that works today.

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
