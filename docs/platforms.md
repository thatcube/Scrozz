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

## Windows first-slice findings

The Windows slice now has a real `HWND` adapter rather than a set of intended
flags. It applies `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED`, removes
`WS_EX_APPWINDOW`, and moves the window with `SetWindowPos(...,
SWP_NOACTIVATE)`. This must be a continuing invariant, not a creation-time
write: winit rewrites the complete `GWL_EXSTYLE` value when egui changes mouse
passthrough. A `WM_STYLECHANGING` guard therefore restores the three required
bits while leaving `WS_EX_TRANSPARENT` under winit's control. While that
transparent bit is set, the same hook returns `HTTRANSPARENT` from
`WM_NCHITTEST`; empty overlay regions pass through and card regions remain
interactive.

Layering has a separate startup requirement. Windows does not guarantee that a
hidden layered window will become visible until `SetLayeredWindowAttributes` or
`UpdateLayeredWindow` initializes it. The Windows host creates its winit window
hidden, installs the style guard, submits a fully transparent bitmap through
`UpdateLayeredWindow`, and only then shows it without activation. It must not
call `SetLayeredWindowAttributes` first: Microsoft documents that a later
`UpdateLayeredWindow` fails until `WS_EX_LAYERED` is cleared and set again.

Native VMware ARM64 runs closed two false renderer fixes. Glow failed before the
first card because the virtual GPU exposed no usable OpenGL swap-control
extension. Switching eframe to wgpu removed that failure, but DX12 selected the
Microsoft Basic Render Driver (WARP), whose ordinary `HWND` surface exposed no
transparent `CompositeAlphaMode`; DWM therefore showed an opaque black
full-screen surface. A successful GPU API initialization was not a successful
transparent overlay.

The Windows host now avoids swap-chain alpha entirely. winit/egui-winit still
own the main-thread event and input model, but Scrozz rasterizes egui's meshes on
the CPU, retains the texture atlas and framebuffer between frames, converts
premultiplied RGBA directly to a persistent top-down 32-bit BGRA DIB, and
presents it with `UpdateLayeredWindow(..., ULW_ALPHA)`. This is independent of
OpenGL, DXGI composition modes, the physical GPU, and WARP. macOS and Linux keep
the qualified eframe/Glow path. Setup or presentation failure is a platform
error; Windows does not silently return to Glow, wgpu, an ordinary opaque
window, or headless mode. Cross-target checks prove the API surface and
host-testable alpha arithmetic. Native WGC evidence has additionally confirmed
the per-pixel desktop composition, work-area placement, unchanged foreground
window, card-only hit testing, resize repaint and clean close path.

The winit event loop, `HWND`, egui input state and layered presenter all stay on
the process main thread. The IPC worker performs framing only and hands bounded
requests to that thread; it never creates or drives a window. `WM_SIZE` also
needs stateful handling: a minimized borderless tool window can report the
system's minimized-icon dimensions as a real resize. Scrozz skips presentation
while the window is iconic, remembers its intended work-area bounds, and
reasserts those bounds exactly once on the minimized-to-restored transition.
Ordinary external resizes remain ordinary resizes.

COM/WinRT membership is per thread, not per process. The direct CLI and the
forwarded-command handler both enter through `commands::dispatch`, which holds
an apartment around backend construction, capture, clipboard delivery and OCR.
The GUI capture worker establishes its own MTA before it reports ready and holds
that guard for the worker's full lifetime. The Windows OCR backend additionally
guards every recognition call, so library callers cannot accidentally reach
`Windows.Media.Ocr` from an uninitialised thread. `RPC_E_CHANGED_MODE` is a retry
signal rather than success: when winit has already selected an STA, Scrozz
retries `RoInitialize(RO_INIT_SINGLETHREADED)` and balances only that successful
WinRT entry, leaving winit's own apartment reference intact.

Windows single-instance forwarding uses a versioned `SCROZZ/2` protocol over a
current-user named pipe. The current token SID is part of both the pipe name and
its protected DACL and is explicitly set as the pipe owner; LocalSystem is the
only additional principal, remote clients are rejected, and
`FILE_FLAG_FIRST_PIPE_INSTANCE` prevents a second server from claiming the
endpoint. Clients use identification-only security quality of service so the
server cannot impersonate them, then verify the connected kernel object's owner
against their current token SID before sending any request bytes. Package
identity does not change that rendezvous: an MSIX and a portable process running
as the same user derive the same endpoint.

Requests and responses are bounded `u32`-length-prefixed frames. Requests are
limited to 1 MiB, combined response output to 512 MiB, transfers to ten seconds,
and command execution to five minutes. Responses preserve exact stdout bytes,
exact stderr bytes and the `u8` exit status, and the server waits for an exact
acknowledgement before disconnecting. Socket/pipe I/O runs on one bounded worker
while the GUI polls a bounded request channel. A forwarded request carries its
deadline and cancellation state into the shared command dispatcher: an expired
queued request is rejected before dispatch, capture delay is interruptible, and
capture output is checked before every side effect. The server cancels execution
at five minutes, then reserves one transfer window for the worker to return that
error; the client additionally reserves the response-transfer window. GUI-only
actions run only after the IPC worker acknowledges that it accepted the command
result. Only `Status::NotRunning` permits
the normal local fallback; an unusable endpoint, framing error or transfer
failure is surfaced rather than silently running the command twice.
`--no-ipc` remains the explicit local-execution override.

Package identity is likewise runtime state, not a Cargo feature or a property of
the executable bytes. Scrozz probes `GetCurrentPackageFullName` and exposes the
result as `data.runtime.package_identity` in capture JSON: `packaged` includes
the full package name, `unpackaged` has none, and `unknown` retains the
unexpected Win32 status and diagnostic. OCR reports its selected engine
separately. An indeterminate answer does not block capture, but OCR fails closed
rather than guessing which identity-sensitive API is safe.

A packaged MSIX or sparse-package process selects `Windows.Media.Ocr`. The exact
same executable launched without identity selects only the Tesseract payload in
its portable artifact; it never searches `PATH`. The portable layout is
`scrozz.exe`, `tesseract/tesseract.exe`, its dependent DLLs, and
`tesseract/tessdata/eng.traineddata` (plus any additional language files the
artifact promises). `SCROZZ_TESSERACT_DIR` is an absolute-path development and
smoke-test override for that `tesseract` directory. A portable ZIP without this
payload is incomplete and reports an actionable unsupported error instead of
silently trying WinRT or returning no text. With no explicit OCR language,
portable Scrozz first tries the Windows locale and then selects the guaranteed
exact `eng` model when no installed traineddata matches it. Explicit language
requests remain authoritative and never silently fall back to English.

The capture backend no longer reads `CO_E_NOTINITIALIZED` or an arbitrary WGC
probe failure as permission to use GDI. GDI is selected only for a genuine
unsupported result or the explicit no-D3D-device case, that downgrade is logged,
and the CLI reports the backend in JSON. A runtime WGC failure is returned
instead of being hidden behind a lower-fidelity frame. GDI also refuses a
visible-cursor request until it can actually composite the cursor.

Finally, the process entry point claims the main/event-loop thread before any
UI object is built. Tray, global-hotkey and window setup can compare against
that identity; the native overlay separately checks that every mutating call is
on the thread that owns its `HWND`. Capture and OCR work belong on initialized
workers, while winit, the tray and window procedures remain on the owner thread
with its message pump.

Promised-file drag is still explicitly unsupported on Windows. The tested
`FILEGROUPDESCRIPTORW` layout is preparation, not delivery: until an
`IDataObject` exposes indexed `CFSTR_FILECONTENTS` through an `IStream` and
`DoDragDrop` reports an accepted drop, a drag-out gesture springs the card back
and retains the capture.

`tools/windows-smoke.ps1` exercises native display enumeration, capture,
encoding, save-once behavior, clipboard round-trip and artifact-selected OCR
while rejecting apartment failures and unexplained GDI downgrades.
`-ArtifactType portable` (the default) asserts `unpackaged` and
`tesseract`; `-ArtifactType packaged` requires `-Binary scrozz.exe`, resolves
that command only as the installed `%LOCALAPPDATA%\Microsoft\WindowsApps`
app-execution alias, invokes the alias token rather than package-directory
bytes, asserts a real package full name and `windows-media-ocr`, and permits
only the documented missing-language-pack skip. `-ExpectedPackageFullName`
optionally requires an exact, ordinal match for the installed package identity
and is rejected for portable runs. Run packaged smoke from a directory without
a portable `scrozz.exe`; a PATH hijack is rejected before launch and the
child-side identity assertion fails closed as a second guard. Packaged mode also
sets `SCROZZ_TESSERACT_DIR` to a unique nonexistent absolute path under its
scratch directory, proving package identity—not an inherited environment
override—selects `Windows.Media.Ocr`.
`-TesseractDirectory` supplies the absolute override for source-built
development artifacts; an extracted portable artifact is validated against its
sibling `tesseract` directory instead. It remains invalid for packaged runs.
Portable mode clears an inherited Tesseract override when none is explicit, and
all modes clear the unstable backend opt-in, so an incomplete ZIP cannot borrow
a developer installation and capture must work with release-default policy.
Before capture it starts the
staged `tesseract.exe --version` with a ten-second timeout and a system-only
`PATH`, which catches a payload whose dependent DLLs were accidentally supplied
only by the packaging machine. `-ExerciseIpc` first refuses an already-running
instance, starts that exact artifact's GUI with a hard timeout, waits for the
default SID-scoped named pipe, and then runs the real capture, clipboard and OCR
work through the GUI command handler. Running that mode once for the portable
artifact and once for the installed package verifies that both package contexts
derive the same per-user endpoint and that forwarded WinRT work enters its COM
apartment. The script deliberately does not claim to automate focus, Alt-Tab
visibility, DWM alpha, Z-order or cross-application hit-testing.
`-RequireWgc` turns a legitimate GDI downgrade into a failure for a Windows 11
WGC qualification run, while `-RequireNegativeCoordinates` requires the lab to
arrange at least one monitor left of or above the primary and proves that its
signed origin survived enumeration.

---

## Known asymmetry, stated honestly

macOS remains the daily development host, so its behavior still receives more
continuous use. The Windows first slice now has native interactive VM evidence
for capture and layered-window behavior, but that qualification is periodic,
not daily use; Linux still lacks equivalent interactive evidence. Layers 1–3
keep platform code from becoming *rot*, but they do not replace recurring
hands-on qualification on every desktop.
