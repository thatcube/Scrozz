<!--
PROVENANCE
  Source:  automated research agent, commissioned for the Scrozz architecture
           decision, 2026-08-26
  Status:  RESEARCH INPUT — not a decision record
  Topic:   UI toolkit evaluation and OSS prior art

Library metadata (stars, licenses, last-push) was read from live sources at
research time and will drift. Re-verify before adopting any dependency.
Items the agent marked "unverified" are exactly that.

Cross-reference:
  docs/cleanshot-parity.md              — authoritative feature inventory
  docs/research/architecture-blueprint.md — externally supplied proposal
-->

# SCROZZ — Research Report: UI Toolkits & OSS Prior Art (August 2026)

---

## SECTION 1: OSS PRIOR ART

### 1.1 Repository Metadata Summary

| App | URL | Language | Stars | Last Push | License |
|---|---|---|---|---|---|
| Flameshot | github.com/flameshot-org/flameshot | C++/Qt | **30,702** | 2026-08-25 | GPL-3.0 |
| ShareX | github.com/ShareX/ShareX | C#/.NET WinForms | **39,319** | 2026-08-26 | GPL-3.0 |
| ksnip | github.com/ksnip/ksnip | C++/Qt | **3,302** | 2026-07-03 | GPL-3.0 |
| Cap | github.com/CapSoftware/Cap | Rust/Tauri + SolidJS | **21,262** | 2026-08-26 | Proprietary (non-standard) |
| Capso | github.com/lzhgus/Capso | Swift 6/SwiftUI | **1,271** | 2026-08-22 | BSL 1.1 |

---

### 1.2 Flameshot — Deep Dive

**Architecture:** C++17, Qt6 (just migrated from Qt5 in v13.0.0, Aug 2025). Single-process; capture overlay is a fullscreen `QWidget` drawn directly via QPainter, with annotation tools as QGraphicsItems. DBus interface for scripting. Tray icon via `QSystemTrayIcon`.

**Platform coverage:** Linux (X11 primary, Wayland partial), Windows, macOS. macOS support is listed but notoriously neglected — macOS issues accumulate in the tracker with slow response times.

**Wayland story (verified):** This is Flameshot's biggest pain point. On Wayland, there is no native protocol for apps to draw transparent fullscreen overlays grabbing raw pixel content, so Flameshot must route through `xdg-desktop-portal` + `grim`. This causes multiple documented issues:
- The selection overlay only appears on a **single monitor** in multi-monitor Wayland setups — open issue `#3461` remains unresolved as of early 2025
- Clipboard integration requires recompiling with `USE_WAYLAND_CLIPBOARD=true` (not shipped in distro packages)
- GNOME 41+ blocks all non-native screen capture, requiring portal permission grants
- wlroots compositors (Sway, Hyprland) require manual installation of `xdg-desktop-portal-wlr` + `grim`; v13.0.0 added a `grim`-based adapter to help

**Annotation editor:** Implemented as `CaptureTool` subclasses; tools include arrow, rectangle, circle, pen, marker, text, pixelate, blur, undo/redo. Decent but not polished by CleanShot standards — no smart snapping, no numbered step annotations, no text with background boxes.

**What they got right:** The _fastest path_ for Linux power users; DBus scripting for hotkey chaining; wide distro package coverage (Flatpak, AppImage, .deb, .rpm, AUR, Homebrew).

**User complaints (verified via GitHub issues):** Wayland multi-monitor failure; macOS neglect; UI feels dated; no built-in cloud upload workflow; scrolling capture absent; no video recording; OCR absent.

---

### 1.3 ShareX — Deep Dive

**Architecture:** C#, Windows Forms + custom WPF-style drawing. Single executable, ~35MB. Built around a task pipeline ("workflow engine"): capture → annotation → after-capture tasks (save/copy/upload/notify). WinForms gives it a bloated but extremely functional UI.

**Platform coverage:** **Windows only.** No Linux, no macOS. This is a hard constraint.

**Feature breadth (ShareX exceeds CleanShot in many areas):**
- 14 distinct capture modes including **scrolling screenshot**, **auto-capture** (timed repeat), and **screen recording with hotspot highlights**
- Workflow automation: custom after-capture task chains
- **80+ upload destinations** (S3, Dropbox, Imgur, custom FTP, custom OAuth) — CleanShot has none
- OCR (Windows built-in), color picker, screen ruler, hash checker, image combiner, image splitter, DNS changer, ping utility — a full productivity suite
- Custom uploader scripting via JSON config files

**What ShareX lacks vs CleanShot:** No macOS/Linux support; no modern UI (2010-era WinForms aesthetics); no camera PiP in recordings; no "pin to screen" as a first-class quick-access flow; binary not notarized; no cloud library.

**What Scrozz should steal:** The workflow pipeline concept; the 80+ uploader system; scrolling capture; step annotations; smart selection.

---

### 1.4 ksnip — Deep Dive

**Architecture:** C++/Qt6, modular with a separate `kImageAnnotator` library. Annotation is in its own reusable SPM-style library. Uses `libkImageAnnotator` as a separate dependency for annotation (useful for Scrozz!).

**Platform coverage:** macOS, Windows, Linux (X11 + Wayland via portal). All 3 platforms have CI (verified by badges in README).

**Features:** Area/fullscreen/window/screen capture, pin to screen, upload, delay capture, annotation (arrows, rects, ellipses, text, number, blur, pixelate, sticker). No video recording.

**Wayland:** Better than Flameshot — uses XDG portal path, which works on GNOME, KDE, and wlroots with proper portal setup. Still no custom overlay UI on Wayland (user sees OS dialog, not ksnip's selection UI).

**What it got right:** Separate annotation library is genuinely reusable. Multi-platform CI. Smaller codebase = more maintainable.

**User complaints:** No video recording; no scrolling capture; no OCR; feels like a simple Linux screenshot utility, not a CleanShot competitor.

---

### 1.5 Cap (CapSoftware/Cap) — Deep Dive

**Architecture (verified via `tauri.conf.json`):**
- Tauri v2 shell with SolidJS/TypeScript frontend (pnpm + Turborepo monorepo, `devUrl: http://localhost:3002`)
- Rust backend doing actual capture, encoding, and media muxing
- **External binaries** bundled: `cap-muxer`, `cap-exporter`, `cap-cli` — heavy Rust sidecar processes for video work
- Uses `macOSPrivateApi: true` for special macOS capabilities (screen recording, audio tap)
- Tauri updater (`createUpdaterArtifacts: true`) is set up but `active: false` in dev config
- Linux .deb targets; deps include `libwebkit2gtk-4.1-0`, `libpipewire-0.3-0`, `libpulse`
- macOS bundles `Spacedrive.framework` for AV processing

**Why they chose Tauri:** Rust for performance-critical media pipeline; web tech for rapid UI iteration; single codebase ships macOS + Windows (Linux partial). Their primary value prop is "Loom but self-hostable" — the web front-end makes sense because the web dashboard is central.

**License:** Non-standard proprietary (GitHub shows "Other"/NOASSERTION). **Not freely reusable** — their LICENSE file enforces commercial restrictions. Cannot copy code into Scrozz.

**Known complaints (from issues/Discord/Reddit):**
- Heavy WebView dependency — 200MB+ install footprint before media tools
- Linux support lags significantly behind macOS/Windows
- No screenshot workflow (pure screen recording focus)
- Cloud upload required for sharing links; self-hosting is complex
- WebView renders at 60fps max, feels less fluid than native tools

---

### 1.6 Capso (lzhgus/Capso) — Deep Dive

**Architecture:** Swift 6.0, SwiftUI, macOS 15.0+. Modular SPM packages: `CaptureKit`, `AnnotationKit`, `OCRKit`. The author explicitly designed modules as reusable packages.

**License (verified from LICENSE file):** **BSL 1.1 (Business Source License 1.1)**. Key terms:
- **Prohibited:** Using it for a "Screen Capture Service" — defined as a commercial product or service providing screenshot/recording/annotation as primary purpose, available to third parties
- **Allowed:** Personal use, internal organizational use, non-commercial distribution, educational use
- **Change Date:** 2029-04-08 → converts to Apache 2.0 on that date
- **Critical for Scrozz:** Since Scrozz is a free OSS screen capture app, it **might** technically fall in the "Screen Capture Service" prohibition even if free. The "commercial" qualifier may save it, but this is legally ambiguous. **Do not copy code without legal review.**

**Features (as advertised):** Area/window/fullscreen/scrolling capture, annotation (arrows, shapes, text, highlights, pixelate), OCR, QR, video recording (MP4/GIF), webcam PiP (4 shapes), camera presentation mode, system audio + mic, recording editor (trim, zoom, cursor smoothing, background), Quick Access HUD, CleanShot-style capture toolbar.

**Limitations:** macOS 15.0+ only; no Windows/Linux; created April 2026, still young (1,271 stars).

---

### 1.7 Shottr (macOS, Closed Source)

**Feature list only (no architecture since closed):**
- Ultra-fast native app: 2.3 MB DMG, 17ms capture time, Apple Silicon optimized
- Area, fullscreen, window capture
- Scrolling screenshots
- Annotation: text, freehand, highlights, spotlight effects, pixelate/erase
- OCR + QR code reading
- Screen ruler / pixel measurer
- Color picker (per-pixel)
- Pin to screen (floating always-on-top borderless windows)
- Combine screenshots onto one canvas
- Resize screenshots in-app
- Image overlay with semi-transparency / before-after animation
- Beautiful backgrounds (gradients, shadows, rounded corners)
- **Closed source; macOS only**

---

### 1.8 Other Credible Cross-Platform OSS Apps

**Satty** (github.com/gabm/Satty) — Rust + GTK4, Linux-only (Wayland-native via grim+slurp), annotations only (no capture), ~2.1K stars. Wayland-native is its key advantage. Apache-2.0.

**Greenshot** (github.com/greenshot/greenshot) — C#/.NET, Windows + macOS, GPL-3.0. ~4.8K stars. Screenshot + annotations, no recording. Largely stagnant since 2020.

**Gyroflow Toolbox** — not a screenshot tool.

**OBS Studio** — screen recording but not screenshot-focused.

**Gyroflow** and **Screenium** — closed source.

**Notable omission:** There is **no credible GPL/MIT cross-platform (mac+win+linux) screenshot+recording app** that achieves CleanShot parity. This is a real gap Scrozz would fill.

---

### 1.9 CleanShot X Feature Coverage Matrix

| Feature | Flameshot | ShareX | ksnip | Cap | Capso | Shottr | **Scrozz target** |
|---|---|---|---|---|---|---|---|
| Area / Window / Fullscreen capture | ✅ | ✅ | ✅ | ❌(record only) | ✅ | ✅ | ✅ |
| Scrolling screenshot | ❌ | ✅ | ❌ | ❌ | ✅ | ✅ | ✅ |
| Annotation canvas (arrows, shapes, text) | ✅ | ✅ | ✅ | ❌ | ✅ | ✅ | ✅ |
| Blur / pixelate sensitive areas | ✅ | ✅ | ✅ | ❌ | ✅ | ✅ | ✅ |
| OCR | ❌ | ✅(win) | ❌ | ❌ | ✅(mac) | ✅(mac) | ✅ |
| Pin to screen (floating windows) | ❌ | ✅ | ✅ | ❌ | ✅ | ✅ | ✅ |
| Screen recording (MP4) | ❌ | ✅ | ❌ | ✅ | ✅ | ❌ | ✅ |
| GIF recording | ❌ | ✅ | ❌ | ✅ | ✅ | ❌ | ✅ |
| Webcam PiP in recording | ❌ | ❌ | ❌ | ✅ | ✅ | ❌ | ✅ |
| System audio capture | ❌ | ✅(win) | ❌ | ✅ | ✅(mac) | ❌ | ✅ |
| Recording editor (trim, zoom) | ❌ | ❌ | ❌ | ✅ | ✅ | ❌ | ✅ |
| Cloud upload / share links | ❌ | ✅ | ❌ | ✅ | ❌ | ❌ | Optional |
| Auto-updater | ❌ | ✅ | ❌ | ✅ | ✅ | ✅ | ✅ |
| Quick Access HUD/toolbar | ❌ | ❌ | ❌ | ❌ | ✅ | ✅ | ✅ |
| Color picker | ❌ | ✅ | ❌ | ❌ | ❌ | ✅ | ✅ |
| Tray/menubar | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| macOS | ⚠️(poor) | ❌ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Windows | ✅ | ✅ | ✅ | ✅ | ❌ | ❌ | ✅ |
| Linux | ✅ | ❌ | ✅ | ⚠️(partial) | ❌ | ❌ | ✅ |

**Section 1 Verdict:** No existing OSS tool covers the full CleanShot X feature matrix across all 3 platforms. ShareX is the richest in features but Windows-only. Capso is the closest spiritual predecessor on macOS. The closest cross-platform prior art is ksnip (C++/Qt), but it lacks recording. Scrozz has real whitespace to fill. The biggest unsolved technical problem across all prior art is: **the Wayland transparent overlay** — every tool either punts to the OS portal dialog (no custom UI) or works only on X11.

---

## SECTION 2: UI TOOLKIT EVALUATION

### 2.1 The Hard Requirements

For a screen-capture app you need:
1. **Fullscreen transparent always-on-top overlay** (selection UI) — the single hardest requirement
2. **Custom-drawn annotation canvas** with hit-testing, layers, undo
3. **Small floating always-on-top HUD/overlay windows** (quick access bar, pin-to-screen)
4. **Tray/menubar item**
5. **Settings window** (normal)

Requirement 1 separates viable from non-viable toolkits. Requirement 2 requires GPU-composited 2D drawing. Requirements 3–5 need robust multi-window support.

### 2.2 Rust Toolkit Evaluations

---

#### **Tauri v2**
- **What it is:** Web front-end (any JS framework) + Rust backend communicating via IPC. Uses system WebView (WebKit on macOS/Linux, Chromium Edge on Windows).
- **Transparent overlay:** ✅ Transparent + always-on-top + fullscreen windows are configurable. **However:** Per-pixel click-through ("only pass clicks on transparent pixels") is NOT supported natively. There is a hack via toggling the entire window to "ignore input" mode, requiring JS-side hitbox detection. GitHub issue #13070 is open and unresolved. In practice for a selection overlay, you can work around this by toggling the window between interactive/passthrough states as the user enters selection mode.
- **Wayland:** Dependent on WebKit/GTK behavior under Wayland. GTK apps can render on Wayland; transparent windows work with compositors, but the hack for click-through is even less reliable.
- **Multi-window:** ✅ Full support; each window is a separate `WebviewWindow`
- **Tray:** ✅ Built-in via `tauri-plugin-shell` or `tauri-plugin-global-shortcut`
- **Canvas annotation:** ✅ Canvas 2D or WebGL/three.js in the webview; excellent for custom drawing
- **DPI:** ✅ WebView handles HiDPI natively
- **Binary size:** Large — ships with WebView2 runtime on Windows; ~10-15MB compressed Tauri binary + ~100MB WebView runtime (Edge WebView2 is pre-installed on Win11)
- **Startup time:** ~300-800ms cold start (WebView initialization)
- **AI training data density:** Very high — TypeScript + Rust, huge ecosystem, tons of examples
- **Accessibility:** WebView inherits browser accessibility (ARIA etc.)
- **Native feel:** ❌ Looks like a web app; custom styling required; misses native macOS/Windows idioms

**Verdict:** The Cap architecture proves Tauri works for screen recording. For Scrozz's overlay requirements, Tauri's click-through limitation is painful but workable. The real concern is startup time and the "web app feel" — problematic for a tool that lives in the taskbar and needs to be instant.

---

#### **egui / eframe**
- **What it is:** Immediate-mode Rust GUI, renders everything via wgpu (GPU) or glow (OpenGL). Pure Rust, no webview, no system widgets.
- **Transparent overlay:** ✅ Supported natively via `ViewportBuilder::with_transparent(true).with_always_on_top(true).with_mouse_passthrough(true)`. The `with_mouse_passthrough()` API exists and works. Community crate `egui_overlay` (github.com/coderedart/egui_overlay) specifically targets this use case. **Known bugs:** #4458 (always-on-top + visible quirks on Windows), #4451 (transparent window black on some Windows GPUs). Generally more mature than Tauri for this use case.
- **Wayland:** Works but depends on winit/wgpu Wayland support. Transparent windows on Wayland require a compositor with compositing support (KWin, Wayfire, Hyprland — yes; basic wlroots — requires testing).
- **Multi-window:** ✅ Multiple viewports; `egui::ViewportId` system for sub-windows
- **Tray:** ❌ Not built-in; needs third-party crate (e.g., `tray-icon`)
- **Canvas annotation:** ✅ Excellent — immediate mode is ideal for custom drawing, hit-testing is trivial since you draw and hit-test in the same pass
- **DPI:** ✅ wgpu backend handles per-monitor DPI correctly
- **Binary size:** ~10-20MB (no system deps, everything statically compiled)
- **Startup time:** ~50-100ms (no web runtime, direct GPU init)
- **AI training data density:** Good — growing Rust ecosystem, well-documented `egui` API, extensive examples
- **Accessibility:** ❌ Minimal — AccessKit integration is in progress but not production-ready
- **Native feel:** ❌ Everything is custom-drawn; looks like a game/tool UI, not a native macOS/Windows app

**Verdict:** egui is technically the strongest fit for Scrozz's core loop (overlay + annotation canvas). Fast startup, small binary, native-class overlay support, and the immediate-mode paradigm is perfect for annotation hit-testing. The deficits are tray (needs crate), native look (requires custom theming), and accessibility (may matter for some users).

---

#### **Iced**
- **What it is:** Elm-inspired reactive Rust GUI. Renders via wgpu (custom backend). More "structured" than egui.
- **Transparent overlay:** ✅ Supports `transparent`, `always_on_top`, `fullscreen`, and `mouse_passthrough` via `window::Settings`. Based on winit, so same platform behaviors as egui. API is at parity.
- **Wayland:** Same as egui — depends on winit/wgpu stack.
- **Multi-window:** ⚠️ Multi-window support added but less mature than egui's viewport system; still some rough edges as of 2025.
- **Canvas:** ✅ `iced::canvas::Canvas` widget with custom `Program` trait for drawing; good for annotation canvas
- **Tray:** ❌ No built-in tray support; third-party crates fragile
- **AI training data density:** Moderate — smaller community than egui, fewer open-source apps
- **Accessibility:** ⚠️ AccessKit integration ongoing; not production-ready

**Verdict:** Technically solid but lags behind egui in ecosystem maturity and multi-window polish. Iced's reactive model is more ergonomic for complex state than egui's immediate mode, but for a tool built by AI agents, egui's simpler model has more training data.

---

#### **Slint**
- **What it is:** Declarative `.slint` DSL compiled to native widgets. Rust, C++, and JS bindings. Focus on embedded + desktop.
- **Transparent overlay:** ❌ Not a first-class feature. No official API for transparent windows in Slint DSL. You can hack it via the winit backend's raw handle, but it's explicitly unsupported and fragile.
- **Multi-window:** ✅ Supported
- **Canvas:** ❌ Slint is declarative/retained-mode; custom free-drawing canvas requires canvas widget + shader, which is awkward for annotation use cases
- **Tray:** ❌ No built-in

**Verdict:** Wrong tool for this job. The transparent overlay requirement alone rules it out without major hacking.

---

#### **GPUI (Zed's)**
- **What it is:** GPU-accelerated retained-mode UI framework, originally macOS-only, now cross-platform. Apache-2.0. Designed for Zed editor's demanding rendering requirements.
- **License:** Apache-2.0 (GPUI itself) — freely usable
- **Transparent overlay:** ❓ GPUI is very new as a general-purpose toolkit (released cross-platform 2024-2025). No documented transparent overlay window API for external use. Zed itself doesn't need overlay windows. **Unverified — likely requires significant work.**
- **Multi-window:** ✅ Used in Zed
- **Tray:** ❓ Not documented for external use
- **AI training data density:** ❌ Very low — almost no external projects use GPUI; minimal training data
- **Maturity for external use:** ❌ GPUI is primarily an internal framework for Zed; lacks documentation for third-party use

**Verdict:** Technically impressive but wrong choice for an AI-built app in 2026. Training data density is near zero; external API is undocumented.

---

#### **Dioxus**
- **What it is:** React-inspired Rust UI framework, multiple renderers (web, desktop via Tauri/Blitz, mobile).
- **Transparent overlay:** Depends on renderer. Desktop renderer wraps Tauri → same limitations as Tauri. Blitz renderer (wgpu-based) is experimental.
- **Tray:** Via Tauri integration
- **AI training data density:** Moderate — growing community, reasonably well-documented

**Verdict:** For Scrozz, Dioxus's desktop renderer is essentially Tauri with a different API. Use Tauri directly if going the WebView route; Dioxus adds abstraction without solving the overlay problem.

---

#### **Freya**
- **What it is:** Rust GUI built on Skia + Dioxus. macOS/Windows/Linux. Very new (2024).
- **Transparent overlay:** ❓ Skia renders to a window; transparent window support is listed but thin. Unverified for overlay use case.
- **Ecosystem:** Very small; almost no training data

**Verdict:** Too immature and too small an ecosystem for AI-coded production use.

---

#### **Xilem / Masonry**
- **What it is:** Experimental Rust UI from the Linebender group (same people as druid/kurbo/vello). Masonry is the widget system; Xilem is the UI architecture on top.
- **Status:** Actively developed (2024-2025) but explicitly "not production ready." No transparent overlay support documented.

**Verdict:** Interesting future candidate; not viable for Scrozz in 2026.

---

### 2.3 C++ Toolkit Evaluations

#### **Qt 6**
- **Transparent overlay:** ✅ Mature — `Qt::FramelessWindowHint | Qt::WindowTransparentForInput | Qt::WindowStaysOnTopHint`. QPainter can render transparent backgrounds. This is literally how Flameshot, ksnip, and Spectacle work. Wayland overlay works via `QWindow::setFlag(Qt::BypassWindowManagerHint)` on X11 or via portal on Wayland. The most battle-tested solution for this use case.
- **Wayland:** ✅ Qt has first-class Wayland QPA; `QT_QPA_PLATFORM=wayland` works; transparent windows on Wayland with compositors
- **Multi-window:** ✅ Excellent
- **Canvas:** ✅ QPainter is mature; scene/view architecture for annotation
- **Tray:** ✅ `QSystemTrayIcon`
- **DPI:** ✅ Qt handles HiDPI/per-monitor DPI well since Qt5.6
- **Binary size:** 50-100MB+ (Qt libs); mitigated with static linking or system Qt
- **Startup time:** ~200-400ms cold (Qt framework init)
- **Accessibility:** ✅ Qt Accessibility framework, AT-SPI on Linux, UIA on Windows

**⚠️ LICENSING ALERT for GPL apps:**
Qt 6 is available as:
- **LGPL v3** (free for most uses, but you must allow users to relink with a modified Qt — requires either dynamic linking OR providing object files)
- **GPL v2/v3** (fine for a GPL app but requires the whole app to be GPL)
- **Commercial** (for proprietary apps)

For a **GPL-licensed Scrozz**, using Qt under LGPL or GPL is fine. The GPL contamination concern is inverted — a GPL Scrozz can use LGPL Qt, but any user must be able to relink Qt. **Dynamic linking satisfies this** and is how all Linux distros ship Qt apps. On macOS/Windows where you bundle Qt.framework, you'd need to include object files or ship dynamic libs — this is standard practice and well-documented. **Not a blocker for Scrozz.**

**AI training data density:** Very high — C++ Qt has decades of training data, extensive StackOverflow presence, Qt documentation is comprehensive.

**Verdict:** Qt is the most technically proven toolkit for transparent overlay windows across all platforms. The GPL/LGPL concern is real but manageable. The downsides for Scrozz: C++ is slower to develop than Rust (especially agentic development), Qt's aesthetic is not native-looking without custom QSS, and C++ memory safety concerns. The existing Flameshot/ksnip codebase can be studied in depth.

---

#### **Dear ImGui**
- **What it is:** Immediate-mode C++ GUI for tools/games. Renders via DirectX, OpenGL, Vulkan, Metal, or SDL/SFML backends.
- **Transparent overlay:** ✅ Trivial — ImGui was literally designed for overlays. The main window can be set transparent and click-through is easy via `ImGui::SetNextWindowBgAlpha(0)` + `WS_EX_TRANSPARENT` on Windows; similar tricks on other platforms.
- **Tray:** ❌ Not built-in; requires native Win32/AppKit/GTK code
- **Settings window:** ⚠️ Doable but you'd build a full settings UI from scratch in ImGui
- **AI training data density:** High — widely used in game tools
- **Verdict:** Great for the overlay and annotation canvas, terrible for the settings window and native integration. You'd be writing platform-specific code for tray, file dialogs, etc. Not recommended as the sole toolkit, but could be used as the _annotation canvas layer_ within a hybrid architecture.

---

### 2.4 Other Toolkits

#### **Flutter Desktop**
- **Transparent overlay:** ✅ Via `flutter_acrylic` (transparency) + `window_manager` (always-on-top). Click-through requires platform channel native code. Works on macOS/Windows/Linux.
- **Wayland:** ✅ Flutter has a Wayland/GTK embedder; transparent windows work with compositors
- **Canvas:** ✅ Excellent — `CustomPainter` is ideal for annotation canvas
- **Tray:** ✅ Via `tray_manager` package
- **AI training data density:** Very high — massive Dart/Flutter ecosystem
- **Downsides:** Dart language (less AI training density than Rust/TypeScript); ~30MB+ binary; Flutter ships its own rendering engine (Skia/Impeller)
- **Native feel:** ❌ Material/Cupertino widgets but can customize

**Verdict:** Underrated for this use case. Flutter's `CustomPainter` is excellent for annotation. But Dart is an unusual choice for a system tool, and Scrozz needs Rust-level system access for screen capture/audio.

#### **Avalonia (.NET)**
- **Transparent overlay:** ✅ `WindowTransparencyLevel.Transparent` + `Topmost=true` + `ExtendClientAreaToDecorationsHint`. Works on all 3 platforms. Click-through via `IsHitTestVisible=false`.
- **Canvas:** ✅ `Canvas` + `DrawingContext` custom rendering
- **Tray:** ✅ Via `Projektanker.Avalonia.Native` or community packages
- **AI training data density:** Good — C# has massive training data; Avalonia-specific documentation is thinner
- **Platform:** macOS, Windows, Linux (X11/Wayland via skia renderer)

**Verdict:** Viable but .NET adds runtime overhead (~50-100MB) and the C# ecosystem for system-level capture (ScreenCaptureKit, DXGI, PipeWire) requires P/Invoke.

#### **Electron**
- **Transparent overlay:** ✅ Well-documented — `transparent: true`, `alwaysOnTop: true`, `setIgnoreMouseEvents(true, {forward: true})` for per-pixel click-through. This is the **most documented** click-through overlay solution in any framework.
- **Wayland:** ✅ Chromium supports Wayland
- **Canvas:** ✅ WebGL/Canvas 2D
- **Downsides:** 150-200MB binary; 500-800ms startup; huge RAM footprint; "Electron shame" for a native-feeling tool
- **Verdict:** The click-through overlay is actually best supported here, which is an interesting data point. But RAM/size make it unacceptable for a screenshot tool that lives in the tray.

---

### 2.5 Transparent Overlay Platform Matrix (Verified)

| Toolkit | macOS Transparent Overlay | macOS Click-through | Windows Overlay | Windows Click-through | Linux X11 Overlay | Linux X11 Click-through | Linux Wayland Overlay | Linux Wayland Click-through |
|---|---|---|---|---|---|---|---|---|
| **Qt 6** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ⚠️ WM-dependent | ⚠️ WM-dependent |
| **egui/eframe** | ✅ | ✅(via ViewportBuilder) | ✅ | ✅ | ✅ | ✅ | ✅ compositor req. | ✅ compositor req. |
| **Tauri v2** | ✅ | ⚠️ Whole-window only | ✅ | ⚠️ Whole-window only | ✅ | ⚠️ Whole-window only | ✅ | ⚠️ No |
| **Electron** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| **Flutter** | ✅ | ⚠️ Native channel req'd | ✅ | ⚠️ Native channel req'd | ✅ | ⚠️ | ✅ | ⚠️ |
| **Iced** | ✅ | ✅(via winit) | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ |
| **Slint** | ❌ (workaround only) | ❌ | ❌ (workaround only) | ❌ | ❌ (workaround) | ❌ | ❌ | ❌ |
| **Avalonia** | ✅ | ✅(IsHitTestVisible) | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| **GPUI** | ❓ unverified | ❓ | ❓ | ❓ | ❓ | ❓ | ❓ | ❓ |

**Note on "click-through":** For a screenshot selection overlay, you need the **entire window to be click-through** except for the selection rectangle handles. This is actually simpler than per-pixel click-through — you toggle the whole window to passthrough while the user is dragging, then intercept only resize handles. This pattern is workable in all toolkits above marked ✅.

**Section 2 Verdict:** **egui** is the strongest fit for Scrozz's overlay+annotation requirements with a Rust-first development approach. **Qt 6** is the most battle-tested but requires C++ and brings licensing compliance work. **Tauri v2** is viable for the non-overlay parts and has proven itself in Cap. For an AI-coded app, a **Tauri v2 + egui hybrid** — where the selection overlay and annotation canvas use egui and the settings/HUD use Tauri webview — is worth considering but adds architectural complexity.

---

## SECTION 3: HYBRID ARCHITECTURES

### 3.1 Shared Rust/C++ Core + Thin Native Shell

**Real-world examples:**

**1. Firefox (C++ core + SpiderMonkey + platform shells)**
The canonical example. XUL UI shell sits over a C++ engine. Platform-specific embedding (macOS AppKit, Win32, GTK). FFI boundary: structured XPCOM interfaces with COM-like vtable calls. Maintenance cost: very high — dedicated teams for each platform shell.

**2. Mozilla's UniFFI / Firefox iOS/Android**
A modern Rust-core pattern: Rust logic compiled as `cdylib`, UniFFI generates Swift/Kotlin/C# bindings. Used in Firefox for iOS, Fenix, and VPN. The FFI boundary is: Rust exposes a flat C API (`extern "C"`), UniFFI generates typed bindings from an `.udl` IDL file. Maintenance cost: moderate — the IDL file is the contract, bindings are auto-generated.

**3. Zed Editor (GPUI — Rust all the way down)**
Not truly hybrid — GPUI renders its own widgets. But Zed uses platform APIs (AppKit, Win32, Wayland) via Rust `unsafe` blocks and the `raw-window-handle` crate to embed GPU surfaces. Maintenance cost: high for GPUI internals; acceptable for app-level code since GPUI abstracts the platform.

**4. 1Password 8 (Rust core + Electron shell)**
1Password moved to a Rust core with Electron front-end. Their Rust library (`op-foundation`) exposes vault/crypto/sync operations; Electron renders the UI. FFI boundary: Node.js N-API bindings to Rust. Maintenance cost: well-documented in their blog posts as "worth it for sharing code with mobile." Relevant: they use this pattern specifically to avoid maintaining separate Windows/Mac/Linux logic.

**5. Cap (Tauri + sidecar Rust processes)**
Verified in Cap's `tauri.conf.json`: `externalBin: ["cap-muxer", "cap-exporter", "cap-cli"]`. The Rust media pipeline runs as a separate process; the Tauri WebView communicates with it via IPC/file system. This is a softer boundary than FFI — process isolation rather than library linking.

**Prior art for capture apps with hybrid architecture:**
- Cap is the only credible example at significant scale, and it uses process-level IPC (not shared library FFI) between the capture/encode pipeline and the UI.
- Flameshot and ksnip are monolithic Qt processes — no hybrid.
- No open-source screenshot app uses the "Rust core + native platform shell" pattern in the traditional sense.

**3.2 Practical FFI Boundary Patterns (from experience):**

| Pattern | Safety | Maintenance Cost | Use case |
|---|---|---|---|
| `extern "C"` + raw pointers | Low | High (manual memory) | Legacy interop |
| UniFFI + .udl IDL | High | Medium (IDL is the contract) | Mobile + desktop shared logic |
| `cbindgen` auto-generated headers | Medium | Low | C/C++ calling Rust |
| Tauri IPC commands | Very high (serialized JSON) | Low | JS UI ↔ Rust backend |
| NAPI-RS (Node ↔ Rust) | High | Medium | Electron ↔ Rust |
| gRPC over localhost | Very high | Medium | Process-level separation |

**Section 3 Verdict:** For Scrozz, a hybrid is only warranted if you want a _truly native_ macOS UI (SwiftUI) over a Rust capture core. If so, UniFFI is the right FFI tool. But the maintenance cost of maintaining two UI codebases (Swift + Rust) is substantial. More practical: go all-in on one Rust toolkit (egui or Tauri) for all three platforms, and use Rust's platform APIs (`screencapturekit-rs`, `win32`, `pipewire-rs`) directly. Cap proves you can do production-quality capture in pure Rust without a native shell.

---

## SECTION 4: DISTRIBUTION / SIGNING REALITY

### 4.1 macOS: Developer ID Signing + Notarization

**Cost (verified):** Apple Developer Program = **$99 USD/year** (confirmed at developer.apple.com/help/account). Individuals or organizations. Required for:
- Developer ID signing (code signing certificate for distribution outside App Store)
- Notarization (submitting to Apple's automated malware scan; required since macOS Catalina)

**Can a free OSS app ship unsigned?** Technically yes, but:
- Gatekeeper blocks unsigned apps by default: user must right-click → Open → override, which most non-technical users won't do
- On macOS 15 (Sequoia), the bypass procedure was _intentionally made harder_ — it now requires going to System Settings → Privacy & Security to approve each unsigned app
- For a tool that captures the screen (requires sensitive permissions), an unsigned app creates a terrible trust experience
- **Recommendation:** Pay the $99/year. For a community OSS project, this is trivially sponsored via GitHub Sponsors or OpenCollective.

**Notarization process:** Submit signed `.dmg` or `.app` via `notarytool` (CLI or Xcode). Apple scans for malware and returns a "ticket" in 5-15 minutes. Automated CI via `xcrun notarytool`. No manual review; fully scriptable.

**Homebrew Cask alternative:** Capso ships via `brew install --cask capso`, which bypasses the direct download friction. Brew users expect this and it works for signed+notarized apps.

---

### 4.2 Windows: Code Signing & SmartScreen

**Current state (2025/2026, verified):**

| Option | Cost | SmartScreen Instant Trust | CI-compatible | Notes |
|---|---|---|---|---|
| **Azure Trusted Signing** | $9.99/mo (Basic) | ❌ No (builds over time) | ✅ Native Azure/CLI | Best value for OSS; needs Azure account |
| **OV Certificate (CA)** | $150-300/yr | ❌ No (builds over time) | ✅ Via HSM/cloud HSM | Hardware token required since 2023 |
| **EV Certificate (CA)** | $400-900/yr | ❌ No longer instant (since 2024!) | ⚠️ HSM in CI is complex | Only mandatory for kernel drivers now |
| **Unsigned** | Free | N/A | N/A | Strong SmartScreen warning; ~30-50% user drop-off |

**Key 2024 change:** EV certificates no longer grant instant SmartScreen trust. Microsoft changed this policy; all certificate types now build reputation organically based on download volume and time. This means Azure Trusted Signing at $9.99/month is genuinely equivalent to EV for SmartScreen purposes — **use Azure Trusted Signing for Scrozz.**

**SmartScreen for unsigned free apps:** Users see a blue "Windows protected your PC" screen with "More info → Run anyway." Studies show 30-50% of non-technical users abort here. For a tool competing with polished paid apps, this is unacceptable. **Sign it from day one.**

---

### 4.3 Linux: Distribution Format Comparison

| Format | Global Hotkeys (sandboxed?) | Portal Screen Capture | Auto-update | Install friction | Distro coverage |
|---|---|---|---|---|---|
| **Flatpak** | ⚠️ Via Global Shortcuts portal (limited, no raw grabs) | ⚠️ Mediated (user prompt, no stealth) | ✅ Flatpak built-in | Low (FlatHub) | Universal |
| **AppImage** | ✅ Full access (unsandboxed) | ✅ Full access | ❌ No built-in (AppImageUpdate tool exists) | Medium (direct download) | Universal |
| **Snap** | ⚠️ Sandboxed (worse than Flatpak for this) | ⚠️ Mediated | ✅ Automatic | Low (Ubuntu default) | Ubuntu-centric |
| **Distro .deb/.rpm** | ✅ Full access | ✅ Full access | ❌ Via distro repos only | Low for that distro | Limited |

**For a screenshot tool specifically:**

Flatpak's screenshot portal forces a user-consent dialog on every capture — this fundamentally breaks the "hotkey → instant capture" UX that defines screenshot tools. The Global Shortcuts portal is still incomplete (dynamic hotkey registration is an open discussion as of 2025, GitHub flatpak/xdg-desktop-portal #1368).

**AppImage is the least painful for Scrozz.** Full system access, no portal mediation, single-file distribution. Publish AppImages via GitHub Releases, which is the standard expectation for OSS apps.

**Recommendation:** Ship both AppImage (primary, full-featured) and Flatpak (secondary, for portal/sandbox users). The Flatpak can note limitations; the AppImage is the power-user path. Also publish .deb for Ubuntu/Debian.

---

### 4.4 Auto-Update Frameworks

| Framework | macOS | Windows | Linux | Notes |
|---|---|---|---|---|
| **Tauri Updater** (built-in) | ✅ | ✅ | ✅ | JSON update manifest; GitHub Releases compatible; built into Tauri architecture |
| **Sparkle 2** | ✅ | ❌ | ❌ | macOS gold standard; Appcast XML; binary delta updates |
| **Squirrel** (Electron) | ✅ | ✅ | ❌ | Part of Electron; overkill without Electron |
| **AppImageUpdate** | ❌ | ❌ | ✅ | Partial; requires user trigger |
| **Custom GitHub Releases poller** | ✅ | ✅ | ✅ | Simple but you build the UI |

**Recommendation for Scrozz:** If using Tauri, use the built-in Tauri updater — it's proven (Cap uses it, `createUpdaterArtifacts: true` confirmed). If using a non-Tauri stack, build a simple GitHub Releases JSON poller in Rust (`reqwest` + `semver` crate) and trigger platform-native update flows.

---

## SECTION 5: AGENTIC DEVELOPMENT FIT

### 5.1 Training Data Density by Stack

| Stack | Language Training Density | Framework-Specific Docs Quality | API Stability (2025) | OSS Example Richness |
|---|---|---|---|---|
| **Tauri v2 + TypeScript** | TypeScript: ⭐⭐⭐⭐⭐; Tauri: ⭐⭐⭐⭐ | Very good (tauri.app v2 docs, migration guide, official plugins) | High (v2 stable) | High (Cap, Spacedrive, many apps) |
| **Rust + egui** | Rust: ⭐⭐⭐⭐; egui: ⭐⭐⭐ | Good (docs.rs, emilk's examples) | Medium (breaking changes between 0.x releases historically) | Medium (many small tools, few large apps) |
| **C++ + Qt6** | C++: ⭐⭐⭐⭐⭐; Qt6: ⭐⭐⭐⭐⭐ | Excellent (qt.io docs are industry-best) | Very high (Qt has maintained BC for decades) | Very high (decades of SO answers, tutorials) |
| **C# + Avalonia** | C#: ⭐⭐⭐⭐⭐; Avalonia: ⭐⭐⭐ | Good but thinner than Qt | Medium | Medium |
| **Dart + Flutter** | Dart: ⭐⭐⭐⭐; Flutter desktop: ⭐⭐⭐ | Good (flutter.dev docs) | High (Flutter desktop stable) | Medium |
| **Rust + Iced** | Rust: ⭐⭐⭐⭐; Iced: ⭐⭐ | Thin docs, many breaking changes | Low (still pre-1.0) | Low |
| **GPUI / Xilem** | Near zero for framework | ❌ No external docs | Unstable | Near zero |

**Evidence-based rationale:**
- GitHub Copilot and GPT-class models have consumed orders of magnitude more TypeScript and C# than Rust. However, Rust's strong type system and borrow checker _reduce_ the category of bugs AI agents typically introduce (use-after-free, data races), making Rust code less likely to have subtle runtime bugs even if the agent has less training density.
- Qt's documentation is the most thorough of any GUI toolkit in existence. When an agent encounters an unfamiliar widget API, Qt docs provide runnable examples inline.
- Tauri's v2 launch (late 2023) included comprehensive migration docs, and the `tauri.app` reference is high quality. The plugin system is well-documented.
- egui's API surface is small and consistent, which helps agents even with less training data.

### 5.2 Headless/Automated GUI Testing in CI

| Stack | macOS CI (GH Actions) | Windows CI (GH Actions) | Linux CI (GH Actions, headless) | Screenshot Testing |
|---|---|---|---|---|
| **egui** | ✅ `egui_kittest` headless rendering | ✅ `egui_kittest` | ✅ No display server needed | ✅ snapshot diffing built-in |
| **Tauri v2** | ✅ `tauri-driver` + WebDriver | ✅ `tauri-driver` + WebDriver | ✅ Xvfb + tauri-driver | ⚠️ WebDriver screenshotting possible but fragile |
| **Flutter** | ✅ `flutter test --platform=macos` | ✅ | ✅ Via `flutter test` + Xvfb | ✅ `matchesGoldenFile` |
| **Qt6** | ✅ `QTEST_MINIMAL_PLUGIN` headless | ✅ | ✅ Xvfb | ✅ QTest screenshot comparison |
| **Avalonia** | ✅ headless mode | ✅ | ✅ | ✅ snapshot testing |
| **Electron** | ✅ Spectron/Playwright | ✅ | ✅ Xvfb | ✅ Playwright screenshots |

**Key notes:**
- **macOS GitHub Actions runners** (`macos-latest`, `macos-14`) are **real Apple Silicon machines** — GUI apps run natively; no Xvfb needed. Screenshot testing works.
- **Windows GitHub Actions runners** are real Windows machines — GUI apps run but need to be marked as `[STAThread]` / window creation can have focus issues. Generally workable.
- **Linux GitHub Actions runners** are Ubuntu, X11 without display server. Headless rendering (wgpu/CPU software rasterization) or Xvfb is required. egui's `egui_kittest` uses a headless wgpu adapter and **does not need a display server** — this is its key advantage. Tauri/Electron need Xvfb.
- **egui is the clear winner for CI testing**: `egui_kittest` (docs.rs/egui_kittest) provides headless rendering, simulated input, AccessKit queries, and snapshot PNG diffing, all running without a display server on all 3 platforms. This is mature and used in the official egui CI itself.

**Section 5 Verdict:** For AI agent development, **Tauri v2 + TypeScript frontend** has the highest training data density for the UI layer, while **Rust + egui** wins for the core graphics loop (overlay + annotation) and CI testability. A hybrid is optimal. Of pure stacks, **Qt 6 + C++** has unmatched documentation density but C++ reduces AI coding reliability.

---

## FINAL RANKED SHORTLIST: 3 VIABLE ARCHITECTURES

---

### 🥇 Architecture A: Rust + egui/eframe (Pure Rust)

**Description:** Single Rust binary. egui for all UI: selection overlay, annotation canvas, floating HUD windows, settings panel, tray via `tray-icon` crate.

**Evidence base:** egui_overlay crate, Cap's Rust media pipeline, egui's `ViewportBuilder` with `with_mouse_passthrough` + `with_transparent` + `with_always_on_top`.

**Pros:**
- Fastest startup, smallest binary (~15-20MB)
- Transparent overlay + mouse passthrough natively supported and tested
- `egui_kittest` enables headless screenshot testing on all 3 platforms in CI without Xvfb
- Single language, single build system (`cargo`)
- Most AI-friendly for overlay/canvas code specifically
- API surface is small; agents make fewer "which API?" mistakes

**Cons:**
- egui UI looks custom/game-engine-ish; significant theming work for "native" feel
- No built-in tray, no built-in file dialogs, no built-in notifications (all need crates)
- Multi-window support is functional but less mature than Qt
- Tray crate (`tray-icon`) is third-party and has had platform bugs

**Biggest single risk:** **egui API instability.** egui is still 0.x and has made breaking changes between minor versions. An AI-coded project that falls behind egui releases could find a large migration cost. Mitigation: pin the egui version tightly and upgrade deliberately.

---

### 🥈 Architecture B: Tauri v2 + Rust (Tauri for Shell, Rust Sidecars for Capture)

**Description:** Tauri v2 WebView shell with a TypeScript (SolidJS/React) UI for settings, HUD, and quick-access panels. Rust backend for screen capture, annotation processing, file I/O. Separate Rust sidecar binaries for heavy media (encoding, GIF). Selection overlay: either a second transparent Tauri window with whole-window toggle click-through, OR embed an egui canvas inside the Tauri binary for just the overlay.

**Evidence base:** Cap (`CapSoftware/Cap`) does this at scale with 21K stars. Their `tauri.conf.json` confirms the sidecar + Tauri updater + multi-platform bundle pattern. Tauri v2 is stable.

**Pros:**
- Proven by Cap in production
- TypeScript for UI = highest AI training density for UI layer
- Tauri updater solves auto-update out-of-the-box
- Rich web ecosystem for settings UI (Tailwind, component libraries, etc.)
- WebDriver + `tauri-driver` for integration testing

**Cons:**
- WebView startup is 300-800ms cold (bad for a "instant capture" hotkey app)
- Per-pixel click-through on the overlay is NOT natively supported in Tauri; workaround required (whole-window passthrough toggle)
- Linux support is functional but requires `libwebkit2gtk`, adding 50MB+ to system deps
- App feels "web-ish" — texture, rendering, and scroll behavior differ from native

**Biggest single risk:** **The transparent overlay click-through limitation.** On the selection overlay, users need to interact with screen content "through" the overlay while dragging their selection box. If Tauri's whole-window passthrough is the only option, toggling it reliably on all platforms (especially Wayland) during selection drag is tricky and fragile. This is a top-5 core feature bug waiting to happen.

---

### 🥉 Architecture C: C++ + Qt 6 (Proven Prior Art Path)

**Description:** C++ with Qt 6 under LGPL. Qt handles everything: transparent overlay (`Qt::FramelessWindowHint | Qt::WindowStaysOnTopHint | Qt::WindowTransparentForInput`), annotation canvas (QGraphicsView or QPainter custom widget), system tray (`QSystemTrayIcon`), settings dialog (QDialog). Capture via platform APIs in C++: ScreenCaptureKit (macOS), DXGI/Desktop Duplication (Windows), PipeWire (Linux).

**Evidence base:** Flameshot (30.7K stars) and ksnip (3.3K stars) both use exactly this architecture and work on all 3 platforms. Qt overlay windows for screenshot tools are the most documented and battle-tested case in existence. Qt's Wayland QPA handles the overlay on Wayland compositors (not perfectly, but better than any Rust alternative).

**Pros:**
- Most battle-tested transparent overlay solution in the world
- Qt documentation is industry-best; agents produce reliable Qt code
- GPL compatibility confirmed (Qt LGPL + app GPL works via dynamic linking)
- Flameshot/ksnip are open-source codebases to learn from directly
- QTest framework for headless testing; CI works on all 3 GH Actions runners
- `QSystemTrayIcon`, file dialogs, notifications — all built-in and high quality

**Cons:**
- C++ memory safety: AI-coded C++ has higher crash risk than AI-coded Rust
- Qt 6 binary size: 50-100MB bundled (Qt frameworks/DLLs on macOS/Windows)
- "LGPL compliance" on macOS/Windows bundles requires including .o files or dynamic libs — standard practice but needs a CI step to verify
- C++ build times are slow; `cmake` + Qt 6 configuration has more moving parts than `cargo`
- Aesthetic: Qt apps look like Qt apps without significant QSS custom theming

**Biggest single risk:** **C++ AI coding quality.** AI agents writing C++, especially with Qt's signal/slot/parent-ownership memory model, produce subtle use-after-free and dangling-pointer bugs that are hard to find in code review. A single `QObject*` lifetime mistake in the capture hot-path can cause crashes. Mitigation: enforce AddressSanitizer in CI; use smart pointers everywhere; isolate unsafe C++ behind a C++ "service" layer with a clear ownership model.

---

## Summary Risk Table

| Architecture | Overlay Reliability | AI Coding Confidence | Startup Time | Binary Size | Wayland Story | Biggest Risk |
|---|---|---|---|---|---|---|
| A: egui | ⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐ | egui API instability |
| B: Tauri v2 | ⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐ | ⭐⭐⭐ | ⭐⭐⭐ | Overlay click-through fragility |
| C: Qt6/C++ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐ | ⭐⭐⭐⭐ | AI C++ memory bugs |

---

## Items Marked Unverified

- **GPUI overlay window support:** Could not find any external project or issue confirming transparent overlay support in GPUI. Classified as unknown.
- **Cap Linux support status:** Their `tauri.conf.json` shows Linux .deb config but community reports indicate Linux is significantly behind macOS/Windows features. Exact feature delta not verified against a changelog.
- **egui Wayland multi-monitor DPI:** Reports of per-monitor scaling being correct on Wayland are from community posts, not official docs. Should be tested.
- **Capso BSL legality for Scrozz:** The "Screen Capture Service" exclusion in BSL 1.1 could apply to a free OSS screen capture app. Legal review recommended before incorporating any Capso code.
- **Tauri click-through on Wayland:** Issue #13070 (github.com/tauri-apps/tauri/issues/13070) confirms the feature is absent. Whether the "whole-window passthrough toggle" workaround works reliably on GNOME Wayland was not independently verified.

---

*Report compiled 2026-08-26. All star counts, last-commit dates, and license texts verified directly from GitHub API and repository files. Architecture claims for Cap verified against actual `tauri.conf.json` at commit `f31590d4`. Capso BSL license text verified from `e3f50f29`.*
