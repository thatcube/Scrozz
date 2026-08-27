# Global shortcuts and screen capture: the per-platform contract

**Provenance.** The table below is Brandon's research (2026-08-26), reproduced as
supplied. The reconciliation beneath it is ours, cross-checked against what the
`shell-tray-hotkey` agent found empirically while implementing
`crates/scrozz-shell/src/hotkey.rs`.

---

## Source table

| Environment | Global shortcut mechanism | Key restrictions / notes | Screen-capture constraints |
| --- | --- | --- | --- |
| **macOS** | Combine `NSEvent.addGlobalMonitorForEventsMatchingMask` with a local monitor for observing key events; use Carbon `RegisterEventHotKey` for robust system-wide hotkeys. | Global monitor is observe-only, does not fire for your own app, and needs Accessibility permission; Carbon hotkeys are still the de-facto system-wide registration API. | Apps can capture via `CGDisplayStream` / `CGWindowListCreateImage`, but global key monitoring and event taps require Accessibility permission and cannot reliably override OS-reserved shortcuts. |
| **Windows** | `RegisterHotKey` defines a system-wide hot key that posts `WM_HOTKEY` to your thread or window even when unfocused. | F12 reserved for the debugger; modifiers and key combos may fail if already registered; `fsModifiers=0` allows single-key grabs, which suppress the key's normal behaviour. | The OS reserves many `Windows`-key combinations (e.g. `Win`+`PrtScn`), so apps should avoid those and expect conflicts; screen capture is unrestricted for desktop apps, but UWP/Store apps have additional constraints. |
| **Linux (X11)** | `XGrabKey` / `XUngrabKey` (or XCB equivalents) after mapping keys via `XKeysymToKeycode`. | Grabs are exclusive and **fail** if another client already grabbed the combination; common keys like `PrintScreen` are usually owned by the DE's screenshot tool. | Capture via X11 (`x11grab`, X server pixel reads) is allowed and common, but not secure; no user-level permission model exists by default. |
| **Linux (Wayland)** | Apps cannot directly listen for global key events; global shortcuts must be mediated by the compositor via `org.freedesktop.portal.GlobalShortcuts` or DE-specific settings. | Traditional app-implemented global hotkeys "won't work" because clients cannot grab keyboard input; portal-based shortcuts move configuration into desktop settings. | Direct capture is prohibited; apps must use xdg-desktop-portal ScreenCast/RemoteDesktop and PipeWire, which show permission dialogs and may not allow persistent input permission. Flameshot documents that Wayland failures are usually portal/permission issues, not app bugs. |

---

## Reconciliation, and the one thing this changes

### macOS is the only platform that lies about conflicts

This is the most useful thing to fall out of comparing the table against our
implementation experience, and it inverts the intuition that conflict handling
should be uniform:

| Platform | On a conflicting registration | Trustworthy? |
|---|---|---|
| **macOS** | `RegisterEventHotKey` returns **`noErr`**. Handler never fires. No API reports it. | **No — it lies** |
| **Windows** | `RegisterHotKey` returns **`FALSE`**, `GetLastError` gives `ERROR_HOTKEY_ALREADY_REGISTERED`. | Yes |
| **X11** | `XGrabKey` produces **`BadAccess`**; grabs are exclusive. | Yes |
| **Wayland** | The compositor mediates, or refuses outright. | Yes, by construction |

Everywhere except macOS, the OS tells you. So the reserved-shortcut table we
built is **not** a general-purpose mechanism to apply uniformly — it is a
macOS-specific workaround for a broken API. On the other three, the correct
behaviour is to *attempt* the registration and report the platform's real error.

Implementing a static table everywhere would be worse than useless: it would go
stale, and it would reject combinations that are actually free on the user's
machine while missing conflicts with whatever third-party tools they run.

### Default hotkeys must be per-platform, and the obvious choice is wrong

Every platform reserves a different set, and the intuitive default collides on
all three:

- **macOS** — `Cmd+Shift+3/4/5` are the built-in screenshot shortcuts.
- **Windows** — `Win`+`PrtScn` is full-screen capture; `Win`+`Shift`+`S` is
  Snipping Tool; **F12 belongs to the debugger**.
- **Linux** — `PrintScreen` is normally already bound to the desktop
  environment's own screenshot tool.

So **`PrintScreen` is a bad default on Linux, and `Cmd+Shift+4` is a bad default
on macOS**, despite each being the "obvious" choice on its platform. Defaults are
chosen per platform from what is actually free, and the first-run experience must
survive the user having taken the combination already.

### The single-key grab footgun

`fsModifiers = 0` on Windows permits grabbing a bare key — and doing so
**suppresses that key's normal behaviour system-wide**. Binding bare
`PrintScreen` would stop `PrintScreen` working for every other application on the
machine. X11's `XGrabKey` behaves comparably. Scrozz must therefore refuse to
bind an unmodified key by default, and if it is ever offered, warn explicitly
about what it takes away.

### Accessibility permission is a hotkey dependency on macOS, not just a capture one

Our permission model (D15) requests capabilities at first use of the feature that
needs them. The table is a reminder that `NSEvent` global monitors need
**Accessibility**, which is a different grant from **Screen Recording**. A user
can perfectly well grant screen recording and still have hotkeys silently dead.
That state must be detected and explained rather than presenting as "the app is
broken".

Carbon `RegisterEventHotKey` does *not* require Accessibility, which is a further
argument for preferring it over an `NSEvent` monitor — fewer permissions, fewer
silent failure modes.

### Wayland: confirms D8 and D11, and adds a supporting citation

Nothing here contradicts our decisions; it strengthens them. Two points worth
carrying:

1. **`org.freedesktop.portal.GlobalShortcuts` exists**, but the `global-hotkey`
   crate does not speak it — the crate is X11-only on Linux and returns `Ok(())`
   on Wayland without binding anything. So the portal is available to us, but
   only via direct D-Bus/`ashpd` work. Worth doing for GNOME and KDE, where it is
   implemented; it remains absent on wlroots, which is why D11 makes the CLI plus
   a compositor keybinding the guaranteed path.
2. **Flameshot documents that Wayland failures are usually portal/permission
   issues rather than app bugs** — which is precisely why D8 requires
   `Error::Unsupported` to carry a truthful `why`. A user who is told "your
   compositor does not implement this, here is the alternative" is informed; a
   user who sees a generic failure concludes the app is broken and leaves.

### Screen capture: one genuine asymmetry to design around

X11 has **no permission model at all** — any client can read the whole screen
silently. macOS and Wayland both gate it. This matters for the permission UI: on
X11 there is no grant to request and no state to display, so the settings surface
must not show a permission row that cannot mean anything there.
