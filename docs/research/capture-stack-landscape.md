<!--
PROVENANCE
  Source:  automated research agent, commissioned for the Scrozz architecture
           decision, 2026-08-26
  Status:  RESEARCH INPUT — not a decision record
  Topic:   Low-level capture / recording / OCR stack landscape

Library metadata (stars, licenses, last-push) was read from live sources at
research time and will drift. Re-verify before adopting any dependency.
Items the agent marked "unverified" are exactly that.

Cross-reference:
  docs/feature-audit.md              — authoritative feature inventory
  docs/research/Cross-Platform Screenshot App Architecture.md — externally supplied proposal
-->

# Scrozz Low-Level Capture/Recording/OCR Stack: Landscape Report (August 2026)

> All star counts are from live GitHub API as of this research session. "Last commit" recency is assessed from repo activity indicators and release dates gathered. All source-code claims are verified against actual file contents fetched above.

---

## SECTION 1 — CROSS-PLATFORM SCREEN CAPTURE LIBRARIES

### Verdict
No single library provides a production-ready, genuinely working single-API abstraction across all four backends (ScreenCaptureKit / WGC / X11 / Wayland+PipeWire). Every project ends up writing per-platform code. The two best Rust candidates for Scrozz are **`xcap`** (stars: 1,007 ✅, Apache-2.0) for static screenshots + window enumeration, and **`scap`** (stars: 635 ✅, MIT) for live frame-streaming. Neither is at v1.0. Cap/scap is actually used in production by the Cap recorder app. `xcap`'s Wayland recording is fully implemented with PipeWire; `scap`'s Linux backend is PipeWire-only but not portal-based. For Scrozz, plan to use **both** as complementary primitives.

---

### 1a. Rust Libraries

#### `scap` — `CapSoftware/scap`
- **URL:** https://github.com/CapSoftware/scap
- **Stars:** 635 | **Forks:** 143 | **License:** MIT | **Version:** 0.1.0-beta.1 (August 2025)
- **Actual backends (verified from `Cargo.toml` and source):**
  - macOS: uses `cidre` crate (a modern ScreenCaptureKit binding) — verified in `Cargo.toml`: `cidre = { version = "0.10.1", features = ["sc", …] }`. Audio capture: `captures_audio: bool` option implemented. ✅
  - Windows: delegates to `windows-capture = "1.5.0"` (Windows.Graphics.Capture API). No DXGI Desktop Duplication path. ✅
  - Linux: `pipewire = "0.8.0"` + `dbus` via xdg-desktop-portal ScreenCast. ✅ (portal-based, requires user approval dialog)
- **Window enumeration:** `get_all_targets()` returns `Display` and `Window` variants on all three platforms. Verified in `src/capturer/mod.rs` — `Target` enum dispatches to platform modules. ✅
- **Known gaps:** Wayland window capture is portal-only (no window-level pick without user dialog). Audio capture is macOS+Windows only (`captures_audio` option documented "only implemented for Windows and macOS currently" — `src/capturer/mod.rs:L76`). Not at 1.0; API stability not guaranteed. 47 open issues.
- **License compat with GPL-3/Apache-2:** ✅ MIT is compatible.

#### `xcap` — `nashaofu/xcap`
- **URL:** https://github.com/nashaofu/xcap
- **Stars:** 1,007 | **Forks:** 140 | **License:** Apache-2.0 | **Version:** 0.9.8 (Rust edition 2024)
- **Actual backends (verified from `Cargo.toml` and source tree):**
  - macOS: `objc2-av-foundation`, `objc2-core-graphics`, `objc2-core-video` — uses AVFoundation/CoreGraphics for screenshots. `src/macos/impl_video_recorder.rs` (20 KB) implements recording. ✅
  - Windows: `windows` crate with both GDI (`Win32_Graphics_Gdi`) and optional WGC (`wgc` feature flag: `Windows.Graphics.Capture`). Default appears to be GDI/BitBlt, WGC is opt-in feature. ⚠️ — GDI misses GPU-composed windows (transparency, DWM).
  - Linux/X11: `xcb` crate with `_NET_CLIENT_LIST_STACKING` for window enumeration (verified from `src/linux/impl_window.rs`). Full capture + recording. ✅
  - Linux/Wayland: **Three-level fallback** (verified from `src/linux/wayland_capture.rs`): (1) `org.gnome.Shell.Screenshot` D-Bus, (2) `org.freedesktop.portal.Screenshot`, (3) `libwayshot-xcap` for wlroots. Screenshot works; **window-level capture on Wayland is NOT implemented** — the `Window::all()` method is X11 XCB only; the Wayland path does monitor/region capture only. ⛔ for window capture on Wayland.
  - Android/HarmonyOS stubs also present in source tree.
- **Video recording:** WIP on all platforms (README says ✅ for screen recording, 🛠️ for window recording on all platforms). The `src/linux/impl_video_recorder.rs` stub exists.
- **Known gaps:** Wayland window enumeration is explicitly ⛔ (per their own README matrix). Windows WGC is a feature flag, not default. Video recording unfinished.
- **License compat:** ✅ Apache-2.0.

#### `screenshots-rs` — `pot-app/screenshots-rs`
- **URL:** https://github.com/pot-app/screenshots-rs
- **Stars:** 1 (pot-app fork) | **License:** Apache-2.0
- **Note:** This is an abandoned fork of nashaofu/xcap from ~2023. The original `nashaofu/screenshots-rs` itself was the predecessor to xcap. **Do not use.** The author explicitly migrated to xcap. The pot-app fork has 0 forks and 1 watcher. ⛔ **Unmaintained.**

#### `libwayshot` — `waycrate/wayshot`
- **URL:** https://github.com/waycrate/wayshot
- **Stars:** 190 | **Forks:** 51 | **License:** BSD-2-Clause | **Latest release:** v1.5.0 (May 2026)
- **Actual backends:** wlroots-family compositors only (wl_output + zwlr_screencopy_v1 protocol). Does NOT work on GNOME or KDE which don't expose `zwlr_screencopy_v1`.
- **xcap uses it:** Verified — `xcap/Cargo.toml` lists `libwayshot-xcap = { git = "https://github.com/nashaofu/wayshot", branch = "main" }` as its third-level Wayland fallback.
- **Window capture:** No. It captures outputs (monitors) and regions. No per-window selection.
- **License compat:** ⚠️ BSD-2-Clause is compatible with GPL-3 and Apache-2, but is different from your target licenses. Permissive, no issue.

#### `screencapturekit-rs` — `doom-fish/screencapturekit-rs`
- **URL:** https://github.com/doom-fish/screencapturekit-rs
- **Stars:** 233 | **Forks:** 46 | **License:** Apache-2.0 | **Active:** yes
- **Actual backends:** macOS only. Safe, idiomatic Rust bindings for Apple ScreenCaptureKit. Supports screen capture, window capture, and audio capture.
- **Relationship to scap:** scap previously used `screencapturekit-rs` but migrated to `cidre` (a lower-level, CapSoftware-forked binding). The commented-out line in Cap's `Cargo.toml` confirms the transition: `# screencapturekit = { git = "https://github.com/CapSoftware/screencapturekit-rs" }`.
- **Verdict:** Still usable standalone for macOS-only work, but scap/cidre is the better-maintained path for macOS capture in 2026.

#### `windows-capture` — `NiiightmareXD/windows-capture`
- **URL:** https://github.com/NiiightmareXD/windows-capture
- **Stars:** 497 | **Forks:** 83 | **License:** MIT | **Active:** yes
- **Actual backends:** Windows only. Uses Windows.Graphics.Capture API (requires Windows 10 1903+). Also wraps DXGI Desktop Duplication for high-performance game capture. Has Python bindings.
- **scap uses it:** Verified from scap `Cargo.toml`: `windows-capture = "1.5.0"` as the Windows backend.
- **Verdict:** Solid Windows-only building block. Not cross-platform itself.

#### `windows-capture` DXGI vs WGC note
- scap/Cap only expose WGC on Windows, not DXGI Desktop Duplication. WGC has a yellow border overlay on captured windows (introduced Windows 11) which is a UX concern for a screenshot app. DXGI avoids the border but is harder to implement for arbitrary app windows.

### 1b. C/C++ Libraries

**OBS Studio (libobs):** https://github.com/obsproject/obs-studio — GPL-2.0 ⛔ for Apache-2.0/GPL-3 project without compatibility analysis. OBS uses: Windows → DXGI Desktop Duplication + WGC selectable; macOS → ScreenCaptureKit (since macOS 13, replaces old AVCapture); Linux → PipeWire via xdg-desktop-portal. Each platform is a separate plugin (`win-capture`, `mac-capture`, `linux-pipewire`). **There is no single cross-platform abstraction** — the plugin system is the abstraction, and it requires the full libobs runtime. Embedding libobs in a non-OBS project is practically infeasible without shipping the entire OBS plugin infrastructure.

**Qt QScreen:** Part of Qt 5/6. `QScreen::grabWindow()` works on X11; on Wayland it falls through to `xdg_desktop_portal` Screenshot. Requires Qt dependency (LGPL-2.1 or commercial). No audio. No recording pipeline. Qt6 `QMediaCaptureSession`/`QScreenCapture` was added in Qt 6.5 and provides cross-platform recording, but Qt itself is a ~100 MB dependency.

**Flameshot:** https://github.com/flameshot-org/flameshot — GPL-3.0. Screenshot-only app, not a library. Uses Qt for capture. On Wayland, uses xdg-desktop-portal Screenshot portal. **Window-level capture not available on Wayland** — Flameshot itself documents this limitation. CLI invocations break on GNOME Wayland because parent window handle is empty (confirmed open issues: #4688, #4600).

**KDE Spectacle / GNOME Screenshot:** Both are end-user apps, not embeddable libraries. Spectacle uses `KWayland` on KDE and falls through to portal on non-KDE. GNOME Screenshot was deprecated in GNOME 42+ in favor of the built-in shell screenshot. Neither is an importable library.

### 1c. Single Cross-Platform API Reality Check
**Verdict: No project has a single truly working API across all 4 backends.** Every library, including scap and xcap, uses compile-time `#[cfg(target_os = ...)]` conditionals routing to separate per-platform implementations. The "cross-platform API" is a thin Rust trait/enum wrapper around four distinct code paths. For Scrozz, you **must accept this architecture** — plan for per-platform feature flags and test on each OS.

---

## SECTION 2 — SCREEN RECORDING / ENCODING

### Verdict
For Rust + cross-platform recording, `ffmpeg-next` (via `CapSoftware/rust-ffmpeg` fork, which Cap uses in production) is the pragmatic choice. Cap/scap proves this works at scale (21k stars, active production use). **H.264 bundled via x264 is legally messy** — use GPU hardware encoders (VideoToolbox/MediaFoundation/VA-API/NVENC) via FFmpeg HW API flags to get H.264 without bundling x264. For a truly clean open-source release, default to H.264 hardware paths only, with AV1/VP9 software fallback.

### 2a. FFmpeg Rust Bindings
- **`ffmpeg-next`** (`zmwangx/rust-ffmpeg`): WTFPL license on the bindings wrapper, FFmpeg itself is LGPL 2.1+ (LGPL build) or GPL (if x264/GPL codecs enabled). Safe Rust wrapper. Cap uses a forked version: `ffmpeg = { git = "https://github.com/CapSoftware/rust-ffmpeg", rev = "49db1fede112" }` (workspace `Cargo.toml` verified). Hardware encoders (VideoToolbox, MediaFoundation, VA-API, NVENC) are accessible through FFmpeg's `AVHWDeviceContext` — you select the encoder name at runtime (`h264_videotoolbox`, `h264_amf`, `h264_nvenc`, `h264_vaapi`, `h264_mf`). No Rust-side changes needed per codec; it's a runtime codec selection.
- Cap's `crates/rendering/Cargo.toml` shows: `ffmpeg.workspace = true`, `wgpu.workspace = true`, metal (macOS), DX12 (Windows) — confirming they use GPU rendering + FFmpeg encoding pipeline.

### 2b. FFmpeg Licensing for a Free App

| Scenario | License Implication |
|---|---|
| LGPL build of FFmpeg, dynamic linking | ✅ App can be GPL-3 or Apache-2. Must ship FFmpeg source or link to it. |
| LGPL build, static linking | ⚠️ Must ship relocatable object files so users can relink. Complex for installers. |
| GPL build (enables x264, libvpx with GPL) | ✅ Compatible with GPL-3, ❌ incompatible with Apache-2.0 standalone. |
| H.264 software via x264 | 🚨 x264 is GPL → forces GPL on your app. Also H.264 patent licensing needed separately. |
| H.264 via hardware encoder (VideoToolbox, MediaFoundation) | ✅ No x264, no GPL contamination. Patent royalty is on encoder hardware vendors, not you. |
| AV1 (libaom/rav1e) | ✅ Free, royalty-free, open. Slower to encode but modern. |

**Recommendation for Scrozz:** Ship FFmpeg as a dynamically linked LGPL build. Use `h264_videotoolbox` (macOS), `h264_mf` (Windows), `h264_vaapi` or `h264_nvenc` (Linux) for hardware H.264. Provide AV1/MP4 (rav1e or libaom) as software fallback. Never ship x264.

### 2c. How OBS and Cap Handle This
- **OBS:** Bundles its own FFmpeg build. Uses VideoToolbox/NVENC/VA-API for hardware H.264. x264 is an optional software encoder. OBS is GPL-2.0 so GPL FFmpeg builds are fine for them.
- **Cap:** Uses a fork of ffmpeg-next (`CapSoftware/rust-ffmpeg`) + `wgpu` for GPU rendering. Audio handled by a custom `cpal` fork (`CapSoftware/cpal`, patched for WASAPI silent-packet zeroing and macOS audio stream stop bug). `cidre` (CapSoftware fork) handles macOS ScreenCaptureKit audio. Cap is Tauri-based (Rust backend + SolidJS frontend). **Cap is macOS+Windows only** — no Linux support in Cap, despite scap having a Linux backend.

### 2d. System/Loopback Audio Capture

| Platform | Mechanism | Rust Library |
|---|---|---|
| macOS (≥13) | ScreenCaptureKit audio capture (per-app or system) OR CoreAudio Process Tap (≥14.4) | `cidre` (Cap's choice), or `scap` `captures_audio` option |
| macOS (<13) | No native loopback — needs BlackHole virtual device | N/A |
| Windows | WASAPI loopback (`AUDCLNT_STREAMFLAGS_LOOPBACK`) | `cpal` (natively supports WASAPI loopback); also direct `windows` crate (verified: Cap's `crates/audio` uses `Win32_Media_Audio` + `cpal`) |
| Linux (PipeWire) | Monitor source on sink node | `cpal` with `pipewire` feature; or native `pipewire` crate |
| Linux (PulseAudio) | `<sink>.monitor` source | `cpal` (PulseAudio backend) |

**Note:** cpal main repo has an open issue #876 ("Support ScreenCaptureKit loopback") and rustdesk-org has a branch `osx-screencapturekit` for this. Cap solved it via their cidre fork. **For Scrozz on macOS, use cidre or replicate Cap's approach.** WASAPI loopback on Windows is well-supported in stock cpal. macOS loopback before 13 is impossible without a virtual driver — document this limitation.

---

## SECTION 3 — WAYLAND FEASIBILITY

### Verdict
Wayland is a **second-class citizen** for a screenshot/capture app in 2026. A basic "capture the screen after user prompt" works. Everything else — window enumeration, global hotkeys on sway/wlroots, click-overlays, synthesized input for scrolling capture, always-on-top positioning — ranges from compositor-specific to outright impossible without privileged access. Plan your Linux support tier accordingly.

### 3a. xdg-desktop-portal ScreenCast Restore Tokens

The `restore_token` field in `org.freedesktop.portal.ScreenCast` allows reuse of a prior session without re-prompting:

| Compositor | Restore Token Support |
|---|---|
| GNOME/Mutter + xdg-desktop-portal-gnome | ✅ Full, stable, "allow always" persists |
| KDE/KWin + xdg-desktop-portal-kde | ✅ Full, stable, persistent session management |
| Hyprland + xdg-desktop-portal-hyprland (XDPH) | ✅ Mostly works; occasional race condition on portal restart losing state; improved significantly through 2025 |
| wlroots/sway + xdg-desktop-portal-wlr | ⚠️ Restore token implemented but portal is less mature; some session persistence issues |
| Cosmic (pop-os) | ⚠️ In progress as of mid-2026 |

**Practical implication:** You can persist ScreenCast permission on GNOME and KDE without reprompting. On Hyprland and sway you should handle graceful re-prompting as a fallback.

### 3b. xdg GlobalShortcuts Portal

| Compositor | GlobalShortcuts Implementation |
|---|---|
| GNOME | Basic/partial — some GNOME Shell security restrictions apply |
| KDE Plasma | ✅ Full — most mature implementation |
| Hyprland (XDPH) | ✅ Full — uses Hyprland-specific protocol (`hyprland-global-shortcuts-v1`) |
| Sway/wlroots | ❌ Not implemented — open issue #240 on xdg-desktop-portal-wlr, no ETA |
| Generic wlroots compositors | ❌ No standard solution |

**Implication for Scrozz global hotkeys (e.g., Cmd+Shift+2 equivalent):** On sway/wlroots compositors, the GlobalShortcuts portal is simply not available. You cannot register a global hotkey programmatically. Users must configure compositor-side keybindings that invoke the Scrozz CLI. This is a known, fundamental limitation.

### 3c. Wayland Feature Feasibility Matrix

| Feature | Wayland Status | Mechanism if possible |
|---|---|---|
| Capture a specific window by app name | ❌ Not programmatic | Portal dialog lets user pick; no enumeration API |
| Enumerate all windows | ❌ Impossible | X11 only via `_NET_CLIENT_LIST_STACKING` (xcap confirmed) |
| Monitor global mouse position | ❌ Impossible without portal | Needs `RemoteDesktop` portal pointer events — not cursor position polling |
| Monitor global keyboard input | ❌ Impossible without portal | `RemoteDesktop` portal can register for events but compositor must grant |
| Synthesize scroll input into another app (scrolling capture) | ❌/🔶 | `RemoteDesktop` portal `NotifyPointerAxis` method — requires user to grant RemoteDesktop permission; compositor support varies |
| Set always-on-top window | ❌ No standard Wayland protocol | wlr-layer-shell for overlays; not portable; KDE/GNOME have `xdg_toplevel` hints but no guarantee |
| Client-side window position control | ❌ Wayland explicitly forbids | compositor decides all window placement |
| Click/keystroke overlays (annotation overlay) | 🔶 Partial | Use layer-shell (wlroots) or xdg_toplevel popups; not universal |
| `libei` / RemoteDesktop portal for input synthesis | 🔶 Emerging | libei implements `ei` protocol; GNOME supports it since ~mutter 44; KDE in progress; wlroots not yet |

**Summary:** On Wayland, Scrozz can do: region/monitor capture (after portal dialog), basic annotation overlays on wlroots via layer-shell, and scrolling capture only on GNOME/KDE where RemoteDesktop portal is implemented. Window-specific capture, global hotkeys (sway), and scrolling capture (sway/wlroots) are not reliably achievable.

### 3d. What Existing Linux Screenshot Tools Actually Do

| Tool | Wayland Approach | Explicitly Unsupported |
|---|---|---|
| **Flameshot** | xdg-portal Screenshot on GNOME/KDE; libwayshot on wlroots. Tray icon works; CLI breaks on GNOME (empty parent window). Known issues #4688, #4600. | Window enumeration, CLI invocation on GNOME, scrolling capture |
| **Spectacle (KDE)** | Works well on KDE via xdg-desktop-portal-kde. Annotation works post-capture. | Window enumeration on non-KDE, global hotkeys on non-KDE |
| **GNOME Screenshot** | Deprecated since GNOME 42. Shell builtin used now. | Everything except basic screenshot |
| **grim + slurp** | grim: wlroots zwlr_screencopy only. slurp: region selector UI. Works great on sway/Hyprland. | GNOME/KDE (no zwlr_screencopy), window capture, portal |
| **ksnip** | Uses Qt screenshot APIs + portal fallback. | Wayland window enumeration |
| **wayshot/libwayshot** | wlroots zwlr_screencopy. Works on sway/Hyprland/river. | GNOME/KDE without wlroots protocol |

**Pattern:** Every tool either (a) targets wlroots-only via `zwlr_screencopy_v1`, or (b) targets portal-only via xdg-portal. Nothing works universally. xcap's three-level fallback (GNOME Shell D-Bus → xdg portal → libwayshot) is the best current Rust implementation of "try everything."

---

## SECTION 4 — OCR

### Verdict
Use native OCR engines per platform (macOS Vision + Windows.Media.Ocr) as the primary path via the **`uniOCR`** abstraction crate, with Tesseract via `leptess` as the Linux fallback. This gives best accuracy + zero extra binary size on macOS/Windows. Do not ship PaddleOCR or Tesseract as primary engines unless Linux is equally important as the others.

### 4a. Native Per-Platform Engines

**macOS Vision (`VNRecognizeTextRequest`):**
- Native, built into macOS 10.15+. Accurate (on par with commercial OCR for Latin scripts). No binary overhead.
- Rust binding: `apple_vision` crate (https://crates.io/crates/apple_vision) provides safe FFI/Swift bridge. Also accessible via `cidre` (which Cap already uses).
- License: Apple proprietary, but you call it via public API — no licensing issue for you.

**Windows `Windows.Media.Ocr`:**
- Native WinRT API, available on Windows 10+. Good accuracy for Latin + many Asian scripts. No binary overhead.
- Rust binding: the `windows` crate already exposes `windows::Media::Ocr::OcrEngine` — no separate dependency needed.

**Linux native OCR:** ❌ **Nothing.** There is no system-provided OCR engine on Linux. You must bundle one.

### 4b. Bundleable Cross-Platform Engines

| Engine | Language | License | Accuracy (Latin) | Accuracy (CJK) | Binary Size | Notes |
|---|---|---|---|---|---|---|
| **Tesseract 5** (LSTM) | C++ | Apache-2.0 ✅ | Good (~95-97% on clean scans) | Moderate | ~15-30 MB (libs + lang data) | Industry standard; requires Leptonica. Rust: `leptess` (MIT), `rusty-tesseract` (MIT). Both require system Tesseract install — not auto-bundled. |
| **PaddleOCR** | Python/C++ | Apache-2.0 ✅ | Excellent | Excellent (purpose-built for Chinese) | 100+ MB with PaddlePaddle runtime | Too heavy for desktop app bundling; Python-first. C++ SDK available but complex. Not recommended for Scrozz. |
| **RapidOCR** (ONNX) | Python/C++ | Apache-2.0 ✅ | Excellent (uses PaddleOCR ONNX models) | Excellent | ~15-27 MB Python wheel; C++ smaller | ONNX Runtime backend, hardware-accelerated. Runs offline. Stars: 7,599 ✅. Active (July 2026). No first-class Rust bindings — would need FFI to C++ or ONNX Runtime Rust API. |
| **ONNX Runtime (direct)** | Any | MIT ✅ | Depends on model | Depends on model | ~5-10 MB runtime + model files | Can run PaddleOCR or other ONNX models directly. Rust: `ort` crate (MIT, https://github.com/pykeio/ort, ~3k stars). |
| **`uniOCR`** (`screenpipe/uniOCR`) | Rust | Apache-2.0 ✅ | Dispatches to native | Dispatches to native | Zero overhead (native) + Tesseract fallback | Stars: 227. Recent project (~2025). Unified API: macOS Vision → Windows.Media.Ocr → Tesseract. This is the correct architecture for Scrozz. |

**uniOCR detail:** https://github.com/screenpipe/uniOCR — 227 stars, Apache-2.0. Provides a single async `OcrEngine::new(OcrProvider::Auto)` API that auto-selects the best available engine. Linux falls back to Tesseract. This is exactly the "native-per-platform with bundled fallback" recommendation.

### 4c. Recommendation
- macOS: `uniOCR` → Apple Vision. Zero binary overhead, best accuracy.
- Windows: `uniOCR` → `Windows.Media.Ocr`. Zero binary overhead, good accuracy.
- Linux: `uniOCR` → Tesseract via `leptess`. ~20-30 MB dependency. Require users to install `libtesseract-dev` (or bundle it in your AppImage/Flatpak).
- **Do not ship PaddleOCR.** Too heavy.
- **Consider RapidOCR via ONNX for a future "offline enhanced" mode** — ships PaddleOCR-quality models via ONNX Runtime (~20 MB) for users who want better CJK/mixed accuracy than Tesseract.

---

## SECTION 5 — SCROLLING CAPTURE (PRIOR ART)

### Verdict
Open-source scrolling capture is immature and all implementations use the same basic technique: (1) synthesize scroll input, (2) capture frames at intervals, (3) align and stitch using image overlap detection. No mature, production-quality Rust implementation exists. You will need to write this from scratch for Scrozz, using platform-specific input synthesis.

### Technique: Input Synthesis + Frame Diff/Stitch
The universal approach across all prior art:
1. User selects the scrollable region/window.
2. App captures an initial frame.
3. App synthesizes a scroll event (mouse wheel or keyboard Page Down).
4. App waits for scroll animation to settle (typically 50-200ms).
5. App captures the next frame.
6. Image alignment: find the overlap between frame N and frame N+1 using template matching (e.g., normalized cross-correlation on a strip near the bottom/top edge). The scroll distance in pixels is the vertical offset where the two frames best align.
7. Crop and stitch: keep only the new (non-overlapping) portion of each subsequent frame.
8. Repeat until no new content detected (last frame matches first) or user stops.

### Prior Art Repos

| Repo | Platform | Language | Technique | Status |
|---|---|---|---|---|
| [Brkgng/ScrollSnap](https://github.com/Brkgng/ScrollSnap) | macOS only | Swift | ScreenCaptureKit + scroll synthesis + smart stitching | Active, macOS 13+ |
| [eurekamaterials/ScrollingScreenshotApp](https://github.com/eurekamaterials/ScrollingScreenshotApp) | macOS only | Swift | Multiple scroll methods + overlap detection | Active, macOS 13+ |
| [JoshuaStorm1017/snagstuff-desktop](https://github.com/JoshuaStorm1017/snagstuff-desktop) | macOS + Windows | Python/PySide6 | PyAutoGUI scroll + frame capture + analysis stitching | Active, cross-platform |
| [jaflo/screenStitch](https://github.com/jaflo/screenStitch) | Any | Python | Manual overlap-based stitching (not auto-scroll) | Old, unrelated to capture |

### Platform-Specific Input Synthesis for Scroll

| Platform | Mechanism | API |
|---|---|---|
| macOS | `CGEventCreateScrollWheelEvent2` (CoreGraphics) | Available from Rust via `core-graphics` crate |
| Windows | `SendInput` with `INPUT_MOUSE` MOUSEEVENTF_WHEEL | Available from Rust via `windows` crate |
| Linux/X11 | `XSendEvent` with button 4/5 (scroll buttons) | `xcb`/`xdo` crate |
| Linux/Wayland | `RemoteDesktop` portal `NotifyPointerAxis` | Must be granted by user; only GNOME/KDE support this reliably |

**Scrolling capture is effectively impossible on Wayland/wlroots** without the RemoteDesktop portal (which sway doesn't implement).

---

## MASTER LIBRARY TABLE

| Name | Repo URL | Language | Platforms (actual, verified) | License | ⭐ Stars | Last Activity | Verdict for Scrozz |
|---|---|---|---|---|---|---|---|
| **scap** | https://github.com/CapSoftware/scap | Rust | macOS (SCK), Windows (WGC), Linux (PipeWire/portal) | MIT ✅ | 635 | Aug 2025 (beta.1) | ✅ Primary for live capture/recording |
| **xcap** | https://github.com/nashaofu/xcap | Rust | macOS, Windows (GDI/WGC), Linux (X11 ✅, Wayland screenshots only) | Apache-2.0 ✅ | 1,007 | Active 2026 | ✅ Primary for screenshots + window enum |
| **screenshots-rs** (pot-app fork) | https://github.com/pot-app/screenshots-rs | Rust | macOS, Windows, Linux | Apache-2.0 ✅ | 1 | 2023 | ❌ Abandoned fork, use xcap instead |
| **libwayshot** | https://github.com/waycrate/wayshot | Rust | Wayland/wlroots only | BSD-2-Clause ✅ | 190 | May 2026 | ✅ As dependency via xcap on sway/Hyprland |
| **screencapturekit-rs** | https://github.com/doom-fish/screencapturekit-rs | Rust | macOS only | Apache-2.0 ✅ | 233 | Active 2025 | 🟡 Superseded by cidre in scap, but usable |
| **windows-capture** | https://github.com/NiiightmareXD/windows-capture | Rust | Windows only (WGC) | MIT ✅ | 497 | Active 2025 | ✅ Used by scap as Windows backend |
| **OBS libobs** | https://github.com/obsproject/obs-studio | C | macOS, Windows, Linux | GPL-2.0 ⚠️ | 62k+ | Active | ⚠️ GPL-2 incompatible with Apache-2 mixing; not embeddable as library |
| **Qt QScreen / QScreenCapture** | https://github.com/qt/qtmultimedia | C++ | macOS, Windows, Linux | LGPL-2.1 or commercial | — | Active | 🟡 ~100MB dep; possible but heavy |
| **Flameshot** | https://github.com/flameshot-org/flameshot | C++ | macOS, Windows, Linux | GPL-3.0 ✅ | 24k+ | Active | ❌ App not library; GPL-3 OK but not embeddable |
| **ffmpeg-next** | https://github.com/zmwangx/rust-ffmpeg | Rust | All (bindings wrapper) | WTFPL ✅ | 1.2k | Active 2025 | ✅ Primary encoding pipeline |
| **cpal** | https://github.com/RustAudio/cpal | Rust | macOS, Windows, Linux | Apache-2.0 ✅ | 2.8k | Active 2025 | ✅ Audio I/O; WASAPI loopback + PipeWire monitor |
| **uniOCR** | https://github.com/screenpipe/uniOCR | Rust | macOS (Vision), Windows (WinRT), Linux (Tesseract) | Apache-2.0 ✅ | 227 | 2025 | ✅ Recommended OCR abstraction |
| **Tesseract 5** (C++) | https://github.com/tesseract-ocr/tesseract | C++ | All | Apache-2.0 ✅ | 63k+ | Active | ✅ Linux OCR fallback via leptess/rusty-tesseract |
| **leptess** | https://github.com/houqp/leptess | Rust | All (requires system Tesseract) | MIT ✅ | ~500 | Moderate | ✅ Rust Tesseract wrapper for Linux |
| **RapidOCR** | https://github.com/RapidAI/RapidOCR | Python/C++ | All | Apache-2.0 ✅ | 7,599 | Active July 2026 | 🟡 No Rust bindings; future "enhanced OCR" mode |
| **rav1e** (AV1 encoder) | https://github.com/xiph/rav1e | Rust | All | BSD-2-Clause ✅ | 3.7k | Active | ✅ Patent-free video encoding alternative to x264 |
| **ScrollSnap** | https://github.com/Brkgng/ScrollSnap | Swift | macOS only | Unknown | ~100 | 2025 | 🟡 Prior art reference only |

---

## TOP 5 BIGGEST RISKS

1. **🔴 Wayland window capture / global hotkeys on sway/wlroots are functionally impossible.** There is no `zwlr_window_list` or similar protocol. `GlobalShortcuts` portal is not implemented in xdg-desktop-portal-wlr. You cannot build CleanShot X-equivalent features on sway/wlroots without compositor-side user configuration. Shipping "Linux" with an asterisk (*requires GNOME or KDE for full feature set) is unavoidable and must be documented.

2. **🔴 FFmpeg H.264 patent + licensing trap.** Bundling H.264 software encoding (x264) forces GPL on your entire codebase AND requires separate MPEG-LA patent licensing for commercial/distributed apps. The safe path — hardware-encoder-only H.264 (VideoToolbox/MediaFoundation/VA-API) — works only on machines with supported GPUs. On headless CI or low-end hardware this will silently fail. You need a software fallback (AV1/rav1e) and clear codec selection logic.

3. **🟠 scap / xcap API instability.** Both crates are pre-1.0. scap is in "beta.1". xcap has 47 open issues. API-breaking changes are likely. Scrozz being AI-agent-implemented makes this riskier — any upstream breaking change will cascade across generated code. Pin exact git SHAs (as Cap does) rather than semver ranges.

4. **🟠 macOS loopback audio requires cidre fork or macOS 13+.** The upstream `cpal` does not yet have stable ScreenCaptureKit loopback support (issue #876 open). Cap solved this via a private fork of cidre and a patched cpal. To capture system audio on macOS you must either replicate Cap's approach (pulling their cidre fork) or drop loopback audio support on macOS < 13. The CoreAudio Process Tap API (macOS 14.4+) is another path but even newer.

5. **🟠 Scrolling capture has no mature cross-platform open-source implementation.** All prior art is macOS-only (Swift) or Python scripts. You will write this from scratch. The hardest sub-problem is Wayland: `RemoteDesktop` portal `NotifyPointerAxis` exists on GNOME/KDE but scroll synthesis on wlroots compositors (sway, Hyprland) has no reliable portal support. Additionally, frame stitching requires careful image alignment to avoid visible seams — naive fixed-offset stitching breaks on pages with varying scroll distances (CSS animations, smooth scrolling). Budget significant development time here.

---

## RECOMMENDED STACK FOR SCROZZ

```
Language:      Rust (primary), with thin Swift/ObjC bridge for macOS-specific APIs

Capture:       scap (CapSoftware/scap) for live recording
               xcap (nashaofu/xcap) for static screenshots + window enumeration on X11
               xcap's Wayland fallback chain (GNOME DBus → xdg-portal → libwayshot) as-is

Encoding:      ffmpeg-next (CapSoftware/rust-ffmpeg fork, pinned rev)
               Hardware H.264: h264_videotoolbox / h264_mf / h264_vaapi / h264_nvenc
               Software fallback: AV1 via rav1e or libaom (Apache-2 / BSD)

Audio:         CapSoftware/cpal fork (WASAPI loopback on Windows, PipeWire monitor on Linux)
               cidre (CapSoftware fork) for macOS ScreenCaptureKit audio

OCR:           uniOCR (screenpipe/uniOCR) → macOS Vision / Windows.Media.Ocr / Tesseract

Scrolling:     Custom implementation: platform input synthesis + frame capture + overlap-stitch
               macOS: CGEventCreateScrollWheelEvent2
               Windows: SendInput MOUSEEVENTF_WHEEL
               Linux/X11: xcb synthetic scroll events
               Linux/Wayland: RemoteDesktop portal (GNOME/KDE only; sway: not supported)

UI framework:  Tauri v2 (Rust+web, as Cap does) OR egui (pure Rust, simpler)
```
