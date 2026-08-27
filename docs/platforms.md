# Platform strategy

**Scrozz is not a macOS app that will be ported later.** Decision D3 makes the
cross-platform core a property of the first commit, and this document is how that
survives contact with the fact that it is being built on a MacBook.

The problem is real and worth stating plainly: code for a platform you cannot run
is code nobody has ever checked. The mitigation is four layers, each catching a
different class of defect, and each one cheaper than the layer below it.

---

## Layer 1 — Cross-target type checking (local, seconds, free)

`cargo check` does not link, so Rust bindings need no Windows SDK or target
linker. It still runs build scripts, which means a dependency that compiles C or
queries target-native libraries can need a foreign toolchain or sysroot even
though no executable is produced. Subject to that exact boundary, the compiler
checks against the real `windows`, `x11rb`, and `ashpd`/zbus APIs.

```bash
tools/check-all-platforms.sh
```

This is the layer that changes the character of the work. Windows and Linux code
is compiled against the genuine API surface, so a misremembered method name, a
wrong argument type, a missing feature flag, or a bad trait bound is a **compile
error on this machine** rather than a surprise days later. It is the difference
between writing platform code blind and writing it with the type checker
watching.

### The build-script exceptions, stated exactly

There are two distinct limitations:

1. **Target C compilation.** `rusqlite`'s `bundled` feature compiles `sqlite3.c`
   for the target. On a foreign target that needs a cross C compiler and sysroot;
   without them the macOS host fails with `fatal error: 'stdlib.h' file not
   found`. `scrozz-store` and the `scrozz` binary that depends on it are therefore
   excluded from cross targets through `SCROZZ_XCHECK_EXCLUDE`. Neither contains
   platform-conditional code, and CI compiles both natively on every runner.
2. **Target `pkg-config` probes.** On Linux, `scrozz-shell` reaches
   libappindicator and GTK through `tray-icon`. The `glib-sys`, `gobject-sys`,
   `gio-sys`, `pango-sys`, and GTK-family build scripts still run during `cargo
   check` and require Linux `.pc` files. A macOS host has neither a Linux
   GLib/GTK sysroot nor target `pkg-config` metadata, so the **full**
   `x86_64-unknown-linux-gnu` workspace check fails there with `pkg-config has
   not been configured to support cross-compilation`. Setting
   `PKG_CONFIG_ALLOW_CROSS=1` is not a fix: it would describe Darwin libraries to
   a Linux target.

`scrozz-shell` is deliberately not put in the standard exclusion list because it
does contain Linux platform code; excluding it would make a full check look green
by hiding the code the command exists to check. The CI gate runs on Ubuntu after
`tools/ci-linux-deps.sh`, so it has native GLib/GObject/GIO/GTK metadata and
checks the full shell. The PipeWire work itself has no build-time system
dependency and passes independently from macOS:

```bash
cargo check --package scrozz-capture --all-targets \
  --target x86_64-unknown-linux-gnu
cargo clippy --package scrozz-capture --all-targets \
  --target x86_64-unknown-linux-gnu -- -D warnings
```

Keeping bundled SQLite is deliberate: shipped builds carry no system SQLite
dependency. Keeping the shell failure visible is equally deliberate: it records
the real limit of a macOS-only cross check rather than claiming coverage that did
not happen.

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

## Worked example: Wayland screen capture

Wayland capture is the sharpest case this document describes, so it is worth
walking through concretely: it has more code that *cannot* be verified from this
machine than anything else in the project, and the split between the layers is
unusually clean.

### How it is built, and why that decision is a platform-strategy decision

Frames arrive over **PipeWire**, because `xdg-desktop-portal`'s `ScreenCast`
interface hands back a *node id*, not pixels. The obvious way to consume that is
the `pipewire` crate — and it is the wrong one here, for two reasons that are
both about this document rather than about ergonomics:

1. **`pipewire-sys` needs `pkg-config` and `bindgen`.** Adding it would take
   `scrozz-capture` — one of the four crates that *must* stay cross-checkable —
   out of layer 1 entirely, in exchange for a Linux sysroot nobody has. The
   single most valuable check in the project would stop covering the largest
   piece of unverifiable code in it.
2. **Linking it puts a `DT_NEEDED` on `libpipewire-0.3.so.0`.** The whole binary
   then fails to load on a machine without PipeWire — including every X11-only
   desktop, where Scrozz otherwise works perfectly.

So PipeWire is **`dlopen`ed at runtime** through `libloading`, and the SPA
parameter objects are encoded by hand in `linux::wayland::pipewire::pod`. The
cost is real: `spa_pod_builder_*` is header-only `static inline`, so there is
nothing to call and the binary format is written out byte by byte. The benefits
are that `cargo check --target x86_64-unknown-linux-gnu` still passes from
macOS with no system packages installed, and that a missing PipeWire degrades to
`Error::Unsupported` naming the package to install rather than to a binary that
will not start.

### What each layer actually proves here

| Layer | Covers | Does not cover |
|---|---|---|
| 1 — cross-check | The ashpd/zbus calls, every signature | Anything behind `dlopen`; symbols are resolved by name at runtime |
| 2 — CI tests | POD encoding byte for byte, format negotiation, stride packing, crop arithmetic, error mapping, lifecycle | Whether a real server accepts any of it |
| 3 — golden images | Nothing; there is no UI in this path | — |
| 4 — real session | Everything below | — |

The unit tests are deliberately literal about the wire format, because a
malformed POD is not rejected with an error — the stream simply never reaches
`Streaming`, which is indistinguishable from a hang.

### The native Linux command CI must run

Layer 1 and layer 2 need nothing new. The runtime library belongs in the Linux
dependency set, and `tools/ci-linux-deps.sh` installs it:

```bash
tools/ci-linux-deps.sh          # includes libpipewire-0.3-0
cargo test --workspace          # the pure Wayland tests run here, headless
```

The end-to-end path needs a real session and is therefore a **separate,
non-blocking** step, run on a native Ubuntu runner:

```bash
tools/ci-linux-deps.sh
tools/wayland-smoke.sh
```

`tools/wayland-smoke.sh` exits **77** — the automake "skipped" convention — with
the specific missing piece on stderr when there is no `WAYLAND_DISPLAY`, no
session bus, no `libpipewire-0.3.so.0`, no PipeWire socket, or no
`org.freedesktop.portal.Desktop` on the bus. It exits 0 **only** when a real
frame with varying pixels was captured. That distinction is the whole point: a
job that skips and exits 0 records a pass for a test that never ran, which is
exactly the failure mode decision D8 exists to prevent. Pass `--require` to turn
every skip into a failure, which is what a dedicated Wayland VM job should do.

Neither the `pipewire` daemon nor any `xdg-desktop-portal-*` backend is
installed by `ci-linux-deps.sh`, because neither can be made to work in a
headless container and installing them would imply otherwise.

### What still needs a real Wayland session

Honestly, and in order of risk:

1. **The C ABI declarations in `pipewire::sys`.** A wrong struct layout compiles
   perfectly and then reads a garbage pointer. Nothing short of running it
   against a real `libpipewire-0.3.so.0` can prove those offsets.
2. **That the hand-encoded POD is one a server accepts.** The tests prove it
   matches the documented format; only a server proves the format was read
   correctly.
3. **That the modifier-less format offer plus the explicit
   `SPA_PARAM_BUFFERS_dataType = MemFd | MemPtr` response produces shared-memory
   buffers**, rather than failing to negotiate or returning DMA-BUF.
4. **The portal dialog**, including that dismissing it produces
   `Error::Cancelled` and not something alarming.
5. **The restore-token round trip**, including the rejection-and-retry path. A
   token that is stored but never accepted is invisible offline and shows up as
   a permission dialog on every single capture.

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
