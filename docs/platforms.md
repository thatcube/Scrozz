# Platform strategy

**Scrozz is not a macOS app that will be ported later.** Decision D3 makes the
cross-platform core a property of the first commit, and this document is how that
survives contact with the fact that it is being built on a MacBook.

The problem is real and worth stating plainly: code for a platform you cannot run
is code nobody has ever checked. The mitigation is four layers, each catching a
different class of defect, and each one cheaper than the layer below it.

---

## Layer 1 — Cross-target type checking (local, seconds, free)

`cargo check` does not link, but it still runs build scripts. Windows Rust code
therefore needs no Windows SDK, while Linux has one important complication:
GTK's `*-sys` crates invoke `pkg-config`, which cannot describe target libraries
without a Linux sysroot.

The cross-platform checker handles both cases. Windows compiles directly
against the real `windows` bindings. Linux overlay code compiles through
`tools/linux-typecheck`, a non-shipping crate that `#[path]`-includes the real
`scrozz-shell/src/linux` modules with their actual `x11rb` and Wayland
dependencies but without the GTK packaging dependency chain.

```bash
tools/check-all-platforms.sh
```

This is the layer that changes the character of the work. Windows and Linux
platform calls are compiled against the genuine API surface, so a misremembered
method name, a wrong argument type, a missing feature flag, or a bad trait bound
is a **compile error on this machine** rather than a surprise days later.

### Build-script limits, stated exactly

There are two limits, both about native build scripts rather than Rust:

1. `rusqlite`'s `bundled` feature compiles `sqlite3.c` *for the target*. Without
   a foreign C toolchain and headers this fails with `fatal error: 'stdlib.h'
   file not found`. `scrozz-store` and the `scrozz` binary are excluded from
   foreign targets via `SCROZZ_XCHECK_EXCLUDE`.
2. `tray-icon` reaches `gtk-sys`, `gdk-sys`, `pango-sys` and friends. Those
   build scripts ask `pkg-config` for target libraries and deliberately refuse
   cross-compilation unless `PKG_CONFIG_SYSROOT_DIR` and a target
   `PKG_CONFIG_PATH` point at a real Linux sysroot. On a non-Linux host,
   `check-all-platforms.sh` therefore checks `scrozz-shell` through
   `tools/linux-typecheck` and excludes the GTK-packaged shell and app from the
   ordinary workspace pass. On Linux, the full workspace compiles normally
   against packages installed by `tools/ci-linux-deps.sh`.

No stub or copied implementation is involved: the shim includes the same
`linux/*.rs` files the shipping crate uses, including the runtime X11 SHAPE and
Wayland protocol calls. What it cannot check is GTK integration or linking.
Real Linux CI does both in layer 2.

Keeping bundled SQLite is deliberate: shipped builds carry no system SQLite
dependency. Installing a Linux sysroot locally is also valid; it is simply not a
prerequisite for checking Scrozz's Rust platform code.

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

The native overlay probes make those manual sessions repeatable and refuse to
masquerade as coverage elsewhere:

```bash
tools/linux-smoke/x11.sh
tools/linux-smoke/kde-wayland.sh
tools/linux-smoke/gnome-wayland.sh
tools/linux-smoke/wlroots.sh
```

Each script prints `SKIP` and exits successfully unless it detects its exact
Linux session. In a matching session, X11 creates a real server-side window and
verifies that two per-card SHAPE rectangles can be replaced by one and then by
an empty input region. KDE and wlroots verify that the compositor accepts
Scrozz's owned **protocol-only** layer surface, while also asserting that the
active eframe renderer remains an ordinary compositor-positioned
`xdg_toplevel`. GNOME asserts D31's compositor-positioned fallback and its
portal explanation. The Wayland probes do not claim rendered layer-shell
coverage; Scrozz still needs an owned renderer before that backend can become
the active surface.

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
