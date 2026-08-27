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

### The exceptions, and why they do not weaken the check

**Crates whose dependencies compile C cannot be cross-checked without a cross C
toolchain.** `cargo check` still runs build scripts, and `rusqlite`'s `bundled`
feature compiles `sqlite3.c` *for the target*, which fails on this machine with
`fatal error: 'stdlib.h' file not found`. `scrozz-store` and the `scrozz` binary
that depends on it are therefore excluded, via `SCROZZ_XCHECK_EXCLUDE`.

This costs nothing real. Only four crates are permitted to contain
`cfg(target_os)` at all — `scrozz-capture`, `scrozz-record`, `scrozz-ocr` and
`scrozz-shell` — and all four still have their portable/default surfaces checked
on all three targets. The native Linux recording feature is the deliberate
exception inside `scrozz-record`: its `pkg-config` build scripts require a real
Linux sysroot for PipeWire, FFmpeg and VA-API, so it remains disabled during a
macOS-hosted cross-check. The pure recording configuration, timing, audio mixing,
format conversion, state and fragmented-MP4 code is still cross-checked
everywhere, while the native feature is compiled, tested and run on Linux in its
own CI lane.

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

Linux recording has an additional native lane. It enables `linux-native` and
`rav1e-fallback`, checks the native feature combinations, runs the recorder
tests and clippy, builds a shared-contract smoke harness, records an Xvfb desktop
through the X11 backend, exercises pause/resume and synchronous finalisation, and
requires `ffprobe` to accept the resulting fragmented MP4. Product ownership
remains in `RecordingMachine`; the smoke does not introduce a second CLI state
machine.

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
- Does a real GNOME or KDE portal report multi-monitor positions in physical
  pixels when fractional scaling is active?
- Do compositor-negotiated DMA-BUF buffers need an import path on a given GPU,
  rather than the directly mapped MemFd/MemPtr path?
- Do the selected VA-API driver and PipeWire microphone/sink-monitor nodes behave
  correctly on the user's hardware?

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

### Testing platform code

8. **`libtest` spawns a thread per test, so no `#[test]` can reach AppKit's main
   thread.** Anything needing the main run loop — `NSApplication`, windows, tray
   items, hotkey managers — is unreachable from an ordinary test. Doctests run on
   the main thread and are the workaround; failing that, test the
   off-main-thread guard and verify real behaviour another way.

### Native Linux recording

9. **The native media dependencies must remain opt-in.** `linux-recording` on the
   app enables `scrozz-record/linux-native`; `rav1e-fallback` separately enables
   the software AV1 fallback. PipeWire, FFmpeg and VA-API all use `pkg-config`, so
   putting them in the default feature set would break the macOS-hosted Linux
   cross-check before rustc reached Scrozz. On Debian/Ubuntu,
   `tools/ci-linux-deps.sh` is the dependency source of truth.
10. **H.264 means the exact `h264_vaapi` encoder, never x264.** `auto` first opens
    `h264_vaapi` against a usable `/dev/dri/renderD*` device. It may fall back only
    to rav1e when `rav1e-fallback` was compiled; an explicit `h264` request fails
    rather than silently selecting a software H.264 encoder.
11. **Wayland recording is portal-owned.** Scrozz negotiates the ScreenCast portal,
    reuses the capture capability probe and restore token, and receives video and
    audio through PipeWire. A rejected restore token is retried once without it,
    and user cancellation remains a cancellation rather than an encoder error.
    X11 uses persistent direct capture and composites the XFIXES cursor itself.
12. **The file is useful before a clean stop.** The muxer writes and syncs the
    ISO-BMFF initialisation segment first, then each `moof`/`mdat` fragment.
    Interrupted sessions report whether the file contains no media, only
    initialisation, or playable fragments, and recovery can identify the valid
    prefix after a torn final fragment.
13. **Some native behaviours remain compositor or hardware validation gaps.**
    PipeWire MemFd and MemPtr buffers are mapped; a compositor that supplies only
    DMA-BUF currently produces an explicit unsupported-buffer error. Portal stream
    positions are treated as physical pixels, so mixed/fractional-scale GNOME and
    KDE layouts still need confirmation on real desktops. ScreenCast also exposes
    no compositor-independent logical backing scale, so `LogicalPoints` currently
    uses an explicit 1:1 assumption on Wayland. The X11/rav1e path is exercised
    under Xvfb in CI, but real portal capture, PipeWire microphone and sink-monitor
    audio, and VA-API encoding still need hands-on runs with the relevant
    compositor, nodes, GPU and driver.

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
