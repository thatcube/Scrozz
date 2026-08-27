# WIP — pinned/floating captures (PIN-01..05, NEW-12)

**Temporary handoff artefact. Delete this file in the commit that lands the
feature.** It exists only because the session implementing pinned captures was
migrated mid-task; it carries the research so the successor does not repeat it.

Branch: `thatcube-pinned-captures`. Base: `d645e86`.
Status at handoff: **no source files written yet.** Everything below is design
and reconnaissance.

---

## Scope

| Item | Feature | Tier |
| --- | --- | --- |
| PIN-01 | Pin screenshot to screen | T1 |
| PIN-02 | Always on top | T1 (M/XL on Wayland) |
| PIN-03 | Adjust size & opacity | T2 |
| PIN-04 | Arrow-key positioning | T3 (XL on Wayland) |
| PIN-05 | Lock mode / click-through | T2 |
| NEW-12 | Pinned chrome: rounded corners / shadow / border toggles | T2 |

Governing decision is **D27**: three surface classes, with pinned captures in
the *transient floating* class. Its three mandatory properties are the
acceptance criteria for this whole feature:

1. small,
2. escapable without documentation,
3. never blocks what is beneath it.

Other binding decisions: D3, D8 (capabilities by **query**, never assumption),
D9 (a window capture is never composited onto — so NEW-12's chrome toggles must
be *locked off* for window-provenance captures), D13 (reduce-motion, a11y),
D19 (motion applies to objects not controls), D23 (pinned captures are never
evicted), D25 (no drawing code reads a clock; goldens render *named instants*),
D28 (bottom-anchored stack), D31 (not yet read — lives past line 937 of
`docs/decisions.md`; **confirm before claiming compliance**).

---

## Planned shape

### 1. Pure model — `crates/scrozz-shell/src/pin/`

Directory module (the crate already uses one for `macos/`). Pure: no platform
calls, no `cfg(target_os)`, fully headlessly testable.

- `mod.rs` — `PinId`, `PinnedSurface` (id, capture, natural size, origin,
  scale, opacity, chrome, `locked`, level, display), `frame()`, `resize`,
  `set_opacity`, `behavior() -> OverlayBehavior`.
- `geometry.rs` — `DisplaySet` over `&[Display]`: `containing(point)`,
  `best_for(rect)` (greatest intersection), `clamp_visible(rect)` keeping a
  minimum number of points on *some* display, and re-snapping to the new
  display's `scale` when a surface crosses a boundary.
- `caps.rs` — `PinCapabilities` / `Support` / `PinBackend`.
- `session.rs` — `PinnedSession`, serde, restore-across-restart reconciliation.

Key types to write:

```rust
pub struct PinChrome { corner_radius: f64, shadow: bool, border: Option<PinBorder> }
pub struct Opacity(f64);   // clamped 0.15..=1.0 — never fully invisible (D27 escapability)
pub struct PinScale(f64);  // clamped ~0.1..=4.0
pub enum NudgeStep { Fine, Normal, Coarse }   // 1pt / 10pt / to display edge
pub enum Support { Yes, Emulated { via: String }, No { why: String, remedy: String } }
pub enum PinBackend {
    MacPanel, WindowsToolWindow, X11OverrideRedirect,
    WaylandLayerShell, WaylandOrdinaryWindow, XWayland, Headless,
}
```

**DPI-correct movement** means: the nudge step is expressed in logical points,
then snapped to whole device pixels on the *target* display, so a 1 pt nudge on
a 2× panel moves exactly 2 physical pixels and never lands on a half-pixel.
`LogicalRect::to_physical` already rounds outwards; reuse it.

**Escapability invariant (PIN-05).** A locked surface is click-through, so
neither pointer nor keyboard can reach it — the escape must come from outside
the surface. Encode this: enumerate `LockEscape` routes (tray item, CLI,
global hotkey) and assert in a test that a locked surface always reports at
least one non-pointer escape route. This is the concrete form of D27's "the
more insistent a window is, the cheaper its escape must be".

### 2. Capabilities — query, never assume (D8)

Derive `PinCapabilities` from the existing `scrozz_shell::hotkey::Session`.
Mirror `Session::from_env(...)`'s deliberately `cfg`-free design so **every
platform branch is reachable from a test on any host** — that is the single
most valuable property of the existing hotkey code and must be preserved here.

- macOS → `MacPanel`; non-activating `NSPanel`, all five capabilities `Yes`.
- Windows → `WindowsToolWindow`; `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW |
  WS_EX_LAYERED` + `HWND_TOPMOST`.
- X11 → `X11OverrideRedirect` + `_NET_WM_WINDOW_TYPE_DOCK`.
- KDE / wlroots (Sway, Hyprland, River, Niri, Wayfire) → `WaylandLayerShell`.
- **GNOME/Mutter Wayland → `WaylandOrdinaryWindow`.** `client_positioning:
  No { why, remedy }`. The compositor places the window; Scrozz must *say so*
  and offer ordinary-window or XWayland behaviour. It must **not** fake
  positioning, and arrow-key nudging must degrade to a stated no-op rather than
  silently doing nothing.

### 3. Native — `crates/scrozz-shell`

- Add `OverlayBehavior::pinned_capture(locked: bool)` alongside the existing
  `capture_card()` / `selection_overlay()` in `src/overlay.rs`.
- macOS backend in `src/macos/overlay.rs` needs **opacity** added (`alphaValue`)
  for PIN-03; `set_frame` / `set_click_through` already exist.
- Non-macOS: `Error::Unsupported` stubs so the workspace compiles everywhere.

### 4. UI — `crates/scrozz-ui/src/pinned.rs`

`#![forbid(unsafe_code)]` crate; must not depend on `scrozz-shell`. Cross the
boundary the way `overlay_app.rs` already does — caller-supplied `PanelHook`
and `PointerProbe` closures.

- Chrome honouring `CardChrome`'s invariant that overlay radius always equals
  thumb radius.
- Keyboard: arrows nudge, Shift/Alt change step, Escape closes.
- Visible lock badge with a cheap escape.
- Accessibility labels (D13).

**Click-through trap.** `mouse_passthrough` is per-window and all-or-nothing.
On macOS `ignoresMouseEvents` means the window receives *no* mouse events, so
egui can never learn the pointer came back. `overlay_app.rs` solves this with a
`PointerProbe` closure and a `RESAMPLE_SECS = 0.35` bounded degradation. This
strongly implies **each pinned capture needs its own OS window** — confirm and
then commit to it.

### 5. Persistence / restore

Set the store's existing `pinned` flag so D23 keeps the pixels. Screen-pin
geometry itself (origin, scale, opacity, chrome, lock) is session state —
decide between `scrozz-store` (SQLite) and `apps/scrozz/src/settings.rs`;
`settings.rs` has **not** been read yet. On restore, reconcile against the
current display set: a saved monitor may be gone, in which case clamp back onto
a surviving display rather than restoring off-screen.

### 6. Wiring

- `OverlayEvent::PinRequested` is currently **dropped on the floor** in
  `apps/scrozz/src/gui/overlay.rs` (~lines 191–195). Primary wiring point.
- `apps/scrozz/src/gui/card.rs::CardEvent` has no `Pin` variant — add it and
  update `CardEvent::card()`.
- `scrozz_ui::card::CardAction::Pin` already exists (slug `"pin"`).
- Tray: `TrayAction` is a 7-variant enum with `ALL`; the test
  `every_tray_item_maps_to_an_action` enforces tray↔`Action` id agreement, so
  adding a tray entry means touching `scrozz_shell::tray` *and*
  `apps/scrozz/src/gui/action.rs` together.
- CLI already has `HistoryCommand::Pin` → slug `"history.pin"`.

### 7. Tests

- Pure state/geometry unit tests (nudging, clamping, multi-display crossing,
  DPI snapping, opacity/scale clamps, chrome-locked-for-window-provenance).
- Capability-matrix test driving `from_env`-style construction across every
  desktop/compositor combination, including the GNOME-Wayland degradation.
- Golden fixtures: add a `pinned-*` `Scenario` to
  `crates/scrozz-ui/src/harness.rs`. Adding a variant to `Scenario::all()`,
  `slug()`, and `Fixture::for_scenario` automatically gets a watermarked
  `PlaceholderScene`; register a real scene via `SceneRegistry::production()`.
  New baselines are generated with
  `UPDATE_SNAPSHOTS=1 cargo test -p scrozz-ui --test golden`.
  **Scenario slugs are the identity of committed baselines — never rename one.**

---

## Reconnaissance notes worth keeping

- **Coordinates.** Scrozz speaks `LogicalRect`: top-left origin of the primary
  display, y down, in points. AppKit is bottom-left, y up, and `origin` names
  the *bottom*-left corner. The bridge is `NSScreen.screens[0].frame.size.height`
  — `frame`, **not** `visibleFrame`. `appkit_to_logical` / `logical_to_appkit`
  already exist and are involutions.
- Floating surfaces anchor to `Display::work_area`, **never** `Display::bounds`
  (raw bounds puts them behind the Dock/taskbar).
- `Display::scale` is per-display. A desktop may mix 2× and 1× panels; there is
  no app-wide scale.
- `OverlayLevel` ordering is the stacking order: `Normal` (D27 default) <
  `Floating` < `Status` < `AboveMenuBar` < `Shielding`.
- Three sources (tray via `muda`, hotkeys via `global-hotkey`, eframe/winit)
  all need the main thread, so `App` is a state machine with one `App::tick()`.
  **Never call `set_event_handler`** on `muda`/`global-hotkey` — it goes into a
  process-global `OnceCell` and the first caller starves the others. Poll and
  drain instead.
- `apps/scrozz` has no windowing dependency on purpose; it defines a narrow
  `CardSurface` trait and `crate::platform::card_surface()` picks the real or
  recording implementation.
- Two distinct `StackLayout` types exist — `scrozz_ui::stack` (egui `Rect`) and
  `scrozz_shell::overlay` (`LogicalRect`).
- `crates/scrozz-ui/src/dock.rs` is `#[path]`-included as `stack::dock`.
- Baseline `cargo check --workspace --all-targets` is **green** at `d645e86`.

## Still unread

`apps/scrozz/src/{settings.rs, gui/app.rs, gui/host.rs, gui/panel.rs,
platform.rs}`, `crates/scrozz-ui/src/theme.rs` (Radius/Elevation tokens for
NEW-12), and decision **D31**.
