<!--
PROVENANCE
  Source:   external research, supplied by @thatcube 2026-08-26
  Status:   UNVERIFIED INPUT — not a decision record
  Verbatim: yes, except base64 image blobs stripped and markdown escape
            artifacts (\+ \- \# \_ \| \[ \]) unescaped

This is one input to the Scrozz architecture decision, recorded as-is so the
reasoning is auditable. It has NOT been fact-checked. Treat its recommendations
as a proposal, not a conclusion.

Claims flagged for verification before anything is built on them:
  - "Core Audio Process Taps introduced in macOS 14.2" — believed to be 14.4
    (CATapDescription). Confirm the exact minimum OS version.
  - Several cited sources are low-authority or possibly non-existent (refs 5, 8,
    19, 21, 24 point at dev.to / lamco.ai / reddit). Independently verify every
    library named before adopting it.
  - Logic-reuse percentages, latency figures, and binary-size ranges in the
    topology comparison table are estimates with no stated methodology.
  - "leveraging Capso's proven architecture" — Capso is BSL 1.1. Its DESIGN may
    be studied; its CODE must not be copied. See docs/cleanshot-parity.md §13.

Cross-reference: docs/cleanshot-parity.md is the authoritative feature inventory,
derived directly from cleanshot.com/features.
-->

# **Technical Architecture and Implementation Blueprint for Scrozz: Engineering a Cross-Platform CleanShot X Parity System**

The desktop productivity ecosystem exhibits a pronounced platform disparity in screen capture and media recording utilities. While macOS benefits from polished commercial tools such as CleanShot X and open-source implementations like Capso, Windows and Linux environments remain fragmented across legacy utilities, unmaintained projects, and web-based wrappers with unoptimized graphical performance1. Developing Scrozz as a high-performance, open-source, multi-platform utility that achieves feature parity with CleanShot X requires an architectural approach that balances cross-platform code reuse with the low-latency graphical demands of native operating system compositors1.

## **CleanShot X Parity Matrix and Systems Audit**

CleanShot X operates as a unified workspace integrating screen capture, vector-based visual annotation, dynamic floating access overlays, real-time scrolling capture, multi-track screen recording, optical character recognition (OCR), and cloud management3. Achieving parity requires mapping each capability to concrete subsystems within Scrozz.

| Feature Domain | CleanShot X Specification | Underlying OS Mechanism | Target Scrozz Architecture |
| :---- | :---- | :---- | :---- |
| **Capture Modes** | Area, Window (with drop shadow & custom padding), Fullscreen, Self-Timer | Window list enumeration, compositor frame scraping, display metric queries3 | Multi-backend capture service (scrozz-capture) with platform-specific window scrapers5 |
| **Scrolling Capture** | Automated vertical/horizontal scrolling capture with real-time stitching | Programmatic scroll event injection, viewport frame sampling, phase correlation6 | Computer vision image stitcher utilizing normalized cross-correlation sliding windows6 |
| **All-in-One HUD** | Transparent interactive overlay with aspect ratio lock, size presets, and loupe magnifier | Transparent multi-monitor overlay window, high-DPI coordinate normalization3 | Hardware-accelerated fullscreen transparent overlay window (Skia/WGPU)5 |
| **Quick Access Overlay** | Floating interactive thumbnail, drag-and-drop source, quick action triggers, auto-close timers | Top-level borderless window, OS drag-source protocol, in-memory pixmap cache1 | Native floating window layer with integrated Drag-and-Drop (NSDraggingSource, IDropTarget, Wayland/X11 DnD)1 |
| **Annotation Canvas** | Non-destructive vector editing: curved arrows, shapes, step counters, highlighter, spotlight, blur/pixelate | Retained-mode vector scene graph, GPU-accelerated shader filters, undo/redo state stacks3 | Rust-based vector scene graph engine with .scrozz project serialization6 |
| **Image Beautification** | Auto-balance margins, gradient/wallpaper backgrounds, padding, corner rounding, drop shadows | 2D raster transformation pipeline, corner clipping shaders, dynamic shadow convolution1 | High-performance raster image transformation pipeline executed via SIMD/WGPU shaders1 |
| **Screen Recording** | 60 FPS video (MP4/H.264), GIF export, system audio loopback, microphone capture, webcam PiP | Hardware encoder integration (NVENC, VideoToolbox, VAAPI), multi-track audio multiplexing3 | Asynchronous frame encoding pipeline with FFmpeg and platform hardware encoder bindings5 |
| **Recording Extras** | Keystroke visualizer overlay, mouse click ripples, cursor smoothing, auto Do Not Disturb | Low-level OS input hooks (CGEventTap, WH_KEYBOARD_LL, libinput), trajectory smoothing3 | Background input event listener with cubic spline cursor path interpolation3 |
| **Video Editor** | Non-destructive trim tool, resolution scaling, stereo-to-mono downmixing, audio gain control | Non-linear demuxing/remuxing engine, audio DSP filter graphs, keyframe-accurate seeking3 | Headless timeline engine generating FFmpeg filter graph commands or native remuxing3 |
| **Floating Pin Window** | Always-on-top floating capture, adjustable opacity, arrow key positioning, lock/click-through mode | Top-level window layering (kCGStatusWindowLevel, WS_EX_TOPMOST, WS_EX_TRANSPARENT)1 | Platform-abstracted floating window manager supporting click-through hit-test masking1 |
| **OCR & Text Recognition** | Instant OCR, visual region highlight, QR code reader, auto-detect language, offline translation | Machine learning text recognition engines, bounding box projection, barcode decoders1 | Multi-backend OCR adapter (VisionKit on macOS, Windows.Media.Ocr, Tesseract/ONNX on Linux)1 |
| **History & Cloud Sync** | Persistent local capture catalog, direct S3/Cloudflare R2 storage upload, link generation | Embedded SQLite database, asynchronous multi-part S3/R2 client, presigned URL generators1 | Local SQLite index store with direct S3-compatible cloud upload engine1 |

The signature user experience of CleanShot X relies on three distinct interaction paradigms: the Quick Access Overlay, the Floating Pin Window, and the Snapping HUD1.  
The Quick Access Overlay functions as an immediate, non-intrusive floating thumbnail that appears after capture1. Rather than blocking the user with standard file dialogs, it provides an immediate drag-and-drop source that can be transferred directly into target applications such as messaging clients or design software without intermediate file system saves1.  
The Floating Pin Window permits any captured visual asset to float above all active applications with adjustable opacity and keyboard-driven pixel positioning1. In its lock mode, it modifies the operating system window style mask to pass all pointer interactions directly through to the underlying application, allowing the pinned image to serve as a visual reference1.  
The Snapping HUD requires near-instantaneous initialization—ideally within 15 to 25 milliseconds—to eliminate perceptible capture lag, matching the display coordinates accurately and querying window hierarchy metadata dynamically to highlight targets beneath the cursor3.

## **Operating System Display and Audio Ingestion Architectures**

The most complex technical layer of a cross-platform capture engine is the capture and audio ingestion subsystem. Each major operating system enforces distinct window management paradigms, compositor protocols, security boundaries, and audio graph architectures5.

+-----------------------------------------------------------------------------------+  
|                            User Interface Layer                                   |  
|   (macOS: SwiftUI/AppKit | Windows: Rust/WinUI | Linux: Rust/GTK4-Libadwaita)     |  
+-----------------------------------------------------------------------------------+  
                                         |  
                                (C-ABI / UniFFI)  
                                         |  
+-----------------------------------------------------------------------------------+  
|                        Core Engine (Rust: scrozz-core)                            |  
|  +---------------------+  +----------------------+  +---------------------------+ |  
|  |  Annotation Engine  |  |  Image Stitching     |  |  OCR & Translation        | |  
|  |  (Vector Graph/WGPU)|  |  (Phase Correlation) |  |  (Vision/WinOCR/ONNX)     | |  
|  +---------------------+  +----------------------+  +---------------------------+ |  
|  +---------------------+  +----------------------+  +---------------------------+ |  
|  |  Encoding Pipeline  |  |  Cloud Sync (S3/R2)  |  |  History Storage (SQLite) | |  
|  |  (FFmpeg/Hardware)  |  |  (Direct Upload)     |  |  (Metadata & Pixmaps)     | |  
|  +---------------------+  +----------------------+  +---------------------------+ |  
+-----------------------------------------------------------------------------------+  
                                         |  
+-----------------------------------------------------------------------------------+  
|                  Platform Capture Abstraction (scrozz-capture)                    |  
|  +----------------------+  +---------------------+  +---------------------------+ |  
|  |   macOS Backend      |  |   Windows Backend   |  |   Linux Backend           | |  
|  | - ScreenCaptureKit   |  | - Win Graphics Cap  |  | - XDG Portal + PipeWire   | |  
|  | - Core Audio Tap     |  | - DXGI Desktop Dup  |  | - X11 XShm (Fallback)     | |  
|  | - CGEventTap Hooks   |  | - WASAPI Loopback   |  | - Pulse/PipeWire Monitor  | |  
|  +----------------------+  +---------------------+  +---------------------------+ |  
+-----------------------------------------------------------------------------------+

### **macOS Ingestion Pipeline**

On macOS, screen capture is orchestrated through Apple's ScreenCaptureKit framework, available on macOS 12.3 and later8. ScreenCaptureKit executes within the window server compositing pipeline, outputting hardware-accelerated CVPixelBuffer frames via zero-copy Metal texture sharing8. This bypasses the severe performance limitations of legacy CGWindowListCreateImage calls, enabling 60 frames-per-second capture at native Retina resolutions8.  
System audio capture relies on Core Audio Process Taps introduced in macOS 14.2, which allow the application to tap the aggregate output device or isolate specific running processes without installing third-party kernel extensions or virtual audio drivers14. OCR and on-device translation are natively offloaded to Apple's Vision (VNRecognizeTextRequest) and Translation frameworks, operating completely offline with minimal resource overhead1.

### **Windows Ingestion Pipeline**

On Windows 10 (version 1903+) and Windows 11, the primary video capture pipeline leverages the Windows.Graphics.Capture (WGC) API alongside the DirectX Graphics Infrastructure (DXGI) Desktop Duplication API5. WGC yields Direct3D 11 textures (IDirect3D11Texture2D), providing hardware-accelerated frame ingestion across discrete graphics adapters8. This framework allows individual windows to be captured cleanly without occlusion from overlapping windows, and supports disabling the system capture border8.  
System audio is captured via the Windows Audio Session API (WASAPI) operating in loopback mode8. A significant engineering hurdle in WASAPI loopback capture is that the audio engine pauses packet generation during complete silence18. If left unhandled, this behavior introduces severe timecode drift between the video track and the audio track18. The capture engine must continuously monitor the AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY flag and run a high-precision multimedia timer to synthesize silent byte buffers during audio stream gaps8. Native OCR is routed directly to the WinRT Windows.Media.Ocr subsystem.

### **Linux Ingestion Pipeline (Wayland and X11)**

The Linux desktop environment presents an architectural divergence between legacy X11 display servers and modern Wayland compositors (such as GNOME Mutter, KDE KWin, and wlroots-based compositors like Sway and Hyprland)4. Under Wayland, application sandboxing restricts arbitrary processes from accessing external screen contents or monitoring global input events directly4.  
Screen ingestion under Wayland must be negotiated via the XDG Desktop Portal ScreenCast interface (org.freedesktop.portal.ScreenCast) over D-Bus19. Upon user authorization, the portal yields a PipeWire node file descriptor19. The capture engine attaches to this PipeWire stream, receiving hardware-allocated DMA-BUF memory pointers that allow direct GPU-accelerated frame consumption without round-tripping through system RAM8.  
To prevent repetitive, disruptive user authorization prompts on every capture event, the engine must persist and supply session restore_token credentials5. On X11 sessions, the engine falls back to the XShmGetImage shared-memory extension with XFixes cursor image composition8. System audio loopback connects to the default audio server's monitor sink via PipeWire or PulseAudio APIs8. OCR processing uses an embedded ONNX runtime or statically linked libtesseract binaries.

| Subsystem Dimension | macOS Pipeline | Windows Pipeline | Linux Pipeline (Wayland / X11) |
| :---- | :---- | :---- | :---- |
| **Video Capture Interface** | ScreenCaptureKit (SCStream)8 | Windows.Graphics.Capture / DXGI5 | XDG ScreenCast Portal + PipeWire / XShm8 |
| **Memory Buffer Architecture** | CVPixelBuffer (Metal zero-copy BGRA)5 | Direct3D 11 Surfaces (DXGI BGRA8)5 | DMA-BUF GPU Pointers / MIT-SHM5 |
| **System Audio Capture** | Core Audio Process Tap (macOS 14.2+)14 | WASAPI Loopback (IAudioClient)8 | PipeWire Node / PulseAudio Monitor Sink8 |
| **Silence Synchronization** | Handled natively by audio clock | Synthetic silence packet injection required18 | Handled via PipeWire graph timebases |
| **OCR Backend** | Apple Vision Framework (VNRecognizeText)1 | Windows.Media.Ocr.OcrEngine | Embedded ONNX Runtime / libtesseract |
| **Window Boundary Discovery** | CGWindowListCopyWindowInfo [cite: 1, 13] | Win32 Desktop Window Manager (DwmGetWindowAttribute) | D-Bus Compositor Interfaces / X11 Tree Queries |

## **Algorithmic Subsystems: Annotations, Stitching, and Media Processing**

A native screenshot utility relies on custom graphical and signal-processing pipelines to handle editing, long-page stitching, and video manipulation with deterministic visual precision across all supported operating systems1.

### **Vector Annotation Scene Graph and Security Shaders**

The annotation canvas must be constructed as a retained-mode vector scene graph rather than an immediate-mode destructive pixel buffer6. When a user draws an arrow, applies a step counter, or places a spotlight, the operation is recorded as a parametric entity within an internal document tree3. This architecture supports infinite undo/redo stacks, dynamic geometry transformations, and project persistence via an open .scrozz serialized format6.  
Security-focused redaction tools require specialized mathematical processing3. Standard mosaic and Gaussian blur algorithms can often be partially inverted using gradient descent or machine-learning de-convolution techniques. To counter this, Scrozz must deploy a secure pixelation shader that couples downsampling with a randomized pixel-shuffling permutation step3.  
Step counters are managed by a centralized sequence coordinator that monitors node relationships, dynamically re-indexing all subsequent counter badges when an intermediate step is removed1. Curved arrows are mathematically defined using quadratic and cubic Bézier splines, computing the tangent vector at the terminal anchor to render sharp, scale-invariant arrow heads dynamically3.

### **Normalized Cross-Correlation for Scrolling Capture**

Scrolling capture automates the capture of expansive web pages, documents, and code windows by tracking viewport movement and stitching sequential frames into a single composite image1. The algorithm operates on continuous frame streams captured during user scrolling or synthetic wheel event injection6:  
*(figure 1 — formula image, omitted)*  
The stitching engine extracts a horizontal reference template strip *(figure 2 — formula image, omitted)* from the base of the current accumulated canvas and evaluates incoming video frames *(figure 3 — formula image, omitted)* across a vertical displacement search space6. The optimal vertical displacement offset *(figure 4 — formula image, omitted)* is determined by locating the maximum cross-correlation coefficient *(figure 5 — formula image, omitted)*6.  
Once the vertical displacement is verified, the system applies a dynamic differential mask to detect and ignore static UI elements—such as sticky navigation headers, persistent sidebars, or floating scrollbars—preventing visual tearing or artifact replication before appending new pixel rows to the primary image buffer.

## **Codebase and Repository Topologies: Monorepo vs. Multi-Repo Analysis**

Choosing an appropriate codebase topology determines long-term maintainability, agentic development efficiency, and run-time UI performance1. The primary options span from completely independent repositories per platform to fully unified cross-platform runtimes.

Repository Strategy Comparison:

[Option A: Polyrepo (3 Repos)]  
Repo 1: Swift / AppKit (macOS)  
Repo 2: C# / WinUI 3 (Windows)  
Repo 3: C++ / Qt6 (Linux)  
--\> Zero Code Reuse | 3x Agent Maintenance Overheads

[Option B: Hybrid Monorepo (Single Repo - Recommended)]  
├── crates/scrozz-core       (Rust: Image Processing, Stitching, OCR, S3)  
├── crates/scrozz-capture    (Rust: Multi-platform OS Capture & Audio)  
├── apps/scrozz-macos        (SwiftUI / AppKit thin native shell)  
├── apps/scrozz-windows      (WinUI 3 / Native Rust GUI shell)  
└── apps/scrozz-linux        (GTK4 / Libadwaita native shell)  
--\> Shared Core (70%) | Native Look, Feel, and Performance (100%)

[Option C: Unified Framework Monorepo (Tauri v2 / Slint)]  
Single Codebase: Rust + Web/Wasm or Rust + Slint UI  
--\> High Code Reuse (90%) | Edge-case complexities with multi-window click-through overlays

| Evaluation Dimension | Option A: Three Polyrepos (Swift, C#, C++) | Option B: Hybrid Monorepo (Rust Core + Native UI Shells) | Option C: Unified App (Tauri v2 + Rust Core) | Option D: Flutter Desktop Monorepo |
| :---- | :---- | :---- | :---- | :---- |
| **Logic Reuse Percentage** | 0% (Complete triplication) | 65% – 75% (Core algorithms, storage, codecs, S3)1 | 85% – 90% (Unified UI layer + Rust backends)24 | 70% – 80% (Dart UI + FFI plugins) |
| **UI Initialization Latency** | Instant (\~15–20ms native launch)9 | Instant (\~15–20ms native launch)9 | Slower (\~80–180ms webview initialization) | Moderate (\~40–70ms canvas engine bootstrap)25 |
| **Transparent Overlay Reliability** | Native window compositor APIs1 | Native window compositor APIs1 | Complex multi-web-view alpha compositing26 | Medium (Plugin dependencies for multi-monitor) |
| **Platform Visual Authenticity** | 100% platform-native1 | 100% platform-native1 | Simulated web components | Canvas-drawn widget imitation25 |
| **Agentic Code Generation Friction** | High (Context switching across 3 languages) | Low (Single Rust core + declarative UI shells)1 | Low (Shared TypeScript and Rust definitions)24 | Medium (Dart FFI synchronization) |
| **Binary Size & Footprint** | Minimal (\~5MB – 15MB)9 | Minimal (\~10MB – 20MB) | Moderate (\~15MB – 35MB)24 | Large (\~40MB – 70MB) |

The evaluation demonstrates that maintaining three entirely disconnected repositories (Option A) introduces severe maintenance overhead. Complex algorithms—such as phase-correlation scrolling capture, non-destructive vector serialization, and S3-compatible cloud engines—would have to be written and debugged three times in Swift, C#, and C++. Conversely, web-based rendering wrappers (Option C) often encounter limitations when managing complex multi-monitor overlays, hardware click-through hit testing, and OS-level drag-and-drop handles without perceptible latency.  
The recommended path is **Option B: A Hybrid Monorepo**. This structure pairs a cross-platform Rust core engine (scrozz-core) with thin, declarative presentation layers tailored to each operating system's interface guidelines1. The Rust core encapsulates all performance-critical, algorithmic, and input-output workloads. The native presentation shells expose the underlying engine through high-performance C-ABI or UniFFI bindings:

> * **macOS Presentation Shell**: Swift 6 using SwiftUI and AppKit, leveraging Capso's proven architecture to deliver liquid visual polish, native menu-bar lifecycles, and Apple Silicon hardware acceleration1.  
> * **Windows Presentation Shell**: Modern Rust with Direct3D/Win32 integration or a thin WinUI 3 wrapper, interfacing natively with the Windows Desktop Window Manager and WASAPI8.  
> * **Linux Presentation Shell**: Rust utilizing GTK4 and Libadwaita, providing seamless integration with modern GNOME, KDE Plasma, and Wayland desktop environments19.

## **Modular Architecture and Agentic Execution Strategy**

To facilitate automated development via autonomous coding agents, the repository must be segmented into modular units with strictly defined interface boundaries1. This layout ensures that agents can implement, test, and refactor discrete modules in isolation without introducing regression errors across unrelated platform layers1.

scrozz/  
├── Cargo.toml                      # Monorepo workspace configuration  
├── crates/  
│   ├── scrozz-core/                # Shared data models, state coordinators, .scrozz format  
│   ├── scrozz-capture/             # Multi-backend screen ingestion engine (WGC, SCK, PipeWire)  
│   ├── scrozz-audio/               # Loopback audio engine (WASAPI, Core Audio, PipeWire)  
│   ├── scrozz-render/              # Vector scene graph, WGPU shaders, image beautifier  
│   ├── scrozz-stitch/              # Scrolling capture computer vision pipeline  
│   ├── scrozz-storage/             # Local SQLite database & direct S3/Cloudflare R2 sync  
│   └── scrozz-ffi/                 # UniFFI / C-ABI bridging layer for frontend shells  
├── apps/  
│   ├── scrozz-macos/               # Native Swift 6 / SwiftUI application shell  
│   ├── scrozz-windows/             # Native Windows presentation layer  
│   └── scrozz-linux/               # Native GTK4 / Libadwaita Linux desktop shell  
└── tests/  
    └── synthetic-harness/          # Headless CI harness simulating screen/audio streams

### **Phased Autonomous Agent Execution Plan**

> 1. **Phase 1: Ingestion Infrastructure and Headless Harnesses** Autonomous agents begin by building crates/scrozz-capture and crates/scrozz-audio8. To decouple capture logic from physical display hardware in continuous integration environments, agents construct a synthetic capture harness capable of emitting dummy video frames and audio buffers at calibrated intervals5. Agents implement platform backends for ScreenCaptureKit, Windows Graphics Capture, and XDG Desktop Portal / PipeWire, ensuring timestamps are normalized across all platforms to a monotonic nanosecond timeline5.  
> 2. **Phase 2: Core Graphics, Vector Canvas, and Stitching Algorithms** Agents implement the vector annotation model in crates/scrozz-render and the phase correlation stitching logic in crates/scrozz-stitch6. Automated test suites supply synthetic scrolling frame sequences, verifying that the stitcher accurately calculates vertical displacement, masks fixed UI headers, and emits intact composite images6. Image beautification pipelines—such as auto-balanced padding, custom drop-shadow convolutions, and secure pixelation shaders—are implemented as reusable WGPU shader pipelines1.  
> 3. **Phase 3: FFI Interface and Native UI Presentation Shells** Agents establish the bridging layer via crates/scrozz-ffi using UniFFI and standard C-ABI declarations24. Agents then construct the native application shells:  
   * The macOS agent builds on the architectural principles established by Capso, connecting SwiftUI views for the Quick Access Overlay, menu bar controller, and floating pin windows to the Rust core1.  
   * The Windows and Linux agents build native system-tray applications, implement borderless transparent overlay windows, and configure click-through hit-test styles (WS_EX_TRANSPARENT on Windows and layer-surface rules under Wayland)3.  
> 4. **Phase 4: Media Encoding, OCR Adapters, and Storage Integration** Agents finalize the recording and export subsystems by connecting FFmpeg and platform-native hardware encoders (Apple VideoToolbox, Nvidia NVENC, Intel/AMD VAAPI) to handle MP4 and GIF generation3. The multi-backend OCR adapter is wired to Apple Vision, Windows OCR, and ONNX runtime models1. Finally, crates/scrozz-storage is integrated to provide local SQLite capture indexing and direct, asynchronous uploads to Cloudflare R2 or custom S3-compatible object storage buckets1.

## **Strategic Synthesis and Production Outlook**

Achieving feature parity with CleanShot X across macOS, Windows, and Linux requires avoiding two common software design traps: rewriting the entire feature stack independently across three disconnected codebases, or forcing a heavy, web-based rendering container across complex operating system boundaries1.  
The hybrid monorepo architecture solves this problem directly. By anchoring image processing, phase-correlation stitching, vector annotations, recording multiplexing, and cloud uploads within a shared Rust core, Scrozz achieves high logic reuse while simplifying maintenance across autonomous development pipelines1. Concurrently, deploying thin, platform-native presentation shells guarantees sub-20-millisecond overlay response times, pixel-perfect window compositing, and seamless system tray integration1. This design provides the foundation for Scrozz to serve as a robust, fully native, open-source screen capture standard across all major desktop environments1.

#### **Works cited**

> 1. lzhgus/Capso: Open-source screenshot and screen ... - GitHub, [https://github.com/lzhgus/Capso](https://github.com/lzhgus/Capso)  
> 2. Capso: Open Source Alternative to CleanShot X, Snagit and Loom, [https://www.opensourcealternatives.to/item/capso](https://www.opensourcealternatives.to/item/capso)  
> 3. All Features - CleanShot X, [https://cleanshot.com/features](https://cleanshot.com/features)  
> 4. Think twice about Wayland. It breaks everything\! - GitHub Gist, [https://gist.github.com/probonopd/9feb7c20257af5dd915e3a9f2d1f2277?permalink_comment_id=5733271](https://gist.github.com/probonopd/9feb7c20257af5dd915e3a9f2d1f2277?permalink_comment_id=5733271)  
> 5. I needed cross-platform screen capture in Rust, so I built pinray, [https://dev.to/agasta/i-needed-cross-platform-screen-capture-in-rust-so-i-built-pinray-4gi](https://dev.to/agasta/i-needed-cross-platform-screen-capture-in-rust-so-i-built-pinray-4gi)  
> 6. Changelog - CleanShot X, [https://cleanshot.com/changelog](https://cleanshot.com/changelog)  
> 7. xcap - crates.io: Rust Package Registry, [https://crates.io/crates/xcap](https://crates.io/crates/xcap)  
> 8. GitHub - Itz-Agasta/pinray: Cross-platform screen & system-audio, [https://github.com/Itz-Agasta/pinray](https://github.com/Itz-Agasta/pinray)  
> 9. Shottr - The Ultimate Mac Screenshot App that is wait... really? FREE, [https://www.reddit.com/r/macapps/comments/s4uvpr/shottr_the_ultimate_mac_screenshot_app_that_is/](https://www.reddit.com/r/macapps/comments/s4uvpr/shottr_the_ultimate_mac_screenshot_app_that_is/)  
> 10. Capso: Free open-source screenshot & screen recorder for Mac, [https://www.producthunt.com/products/capso](https://www.producthunt.com/products/capso)  
> 11. I built an embeddable Windows screen + system-audio capture, [https://www.reddit.com/r/rust/comments/1vu7we9/i_built_an_embeddable_windows_screen_systemaudio/](https://www.reddit.com/r/rust/comments/1vu7we9/i_built_an_embeddable_windows_screen_systemaudio/)  
> 12. I built macOS screenshot utility for UI developers : r/webdev - Reddit, [https://www.reddit.com/r/webdev/comments/qe92si/i_built_macos_screenshot_utility_for_ui_developers/](https://www.reddit.com/r/webdev/comments/qe92si/i_built_macos_screenshot_utility_for_ui_developers/)  
> 13. cleanshot-alternative · GitHub Topics, [https://github.com/topics/cleanshot-alternative](https://github.com/topics/cleanshot-alternative)  
> 14. clipclip - Rust - Docs.rs, [https://docs.rs/clipclip](https://docs.rs/clipclip)  
> 15. RustAudio/cpal: Low-level cross-platform audio I/O library in Rust, [https://github.com/RustAudio/cpal](https://github.com/RustAudio/cpal)  
> 16. windows::Graphics::Capture - Rust, [https://microsoft.github.io/windows-docs-rs/doc/windows/Graphics/Capture/index.html](https://microsoft.github.io/windows-docs-rs/doc/windows/Graphics/Capture/index.html)  
> 17. wasapi - crates.io: Rust Package Registry, [https://crates.io/crates/wasapi](https://crates.io/crates/wasapi)  
> 18. How to record audio with WasapiLoopbackCapture when no voice is, [https://stackoverflow.com/questions/52345617/how-to-record-audio-with-wasapiloopbackcapture-when-no-voice-is-coming-out-from](https://stackoverflow.com/questions/52345617/how-to-record-audio-with-wasapiloopbackcapture-when-no-voice-is-coming-out-from)  
> 19. Rust libraries for Wayland screen capture and video processing, [https://lamco.ai/open-source/lamco-wayland/](https://lamco.ai/open-source/lamco-wayland/)  
> 20. If you're running a modern Linux desktop you're probably running, [https://news.ycombinator.com/item?id=22748897](https://news.ycombinator.com/item?id=22748897)  
> 21. lamco_portal - Rust - Docs.rs, [https://docs.rs/lamco-portal](https://docs.rs/lamco-portal)  
> 22. Making Upwork Screen Capture Work on Wayland | Daniel Moretti, [https://danielmoretti.com/blog/making-upwork-screen-capture-work-on-wayland](https://danielmoretti.com/blog/making-upwork-screen-capture-work-on-wayland)  
> 23. I was having issues with existing screen capture crates on Hyprland, [https://www.reddit.com/r/rust/comments/1uqwc7s/i_was_having_issues_with_existing_screen_capture/](https://www.reddit.com/r/rust/comments/1uqwc7s/i_was_having_issues_with_existing_screen_capture/)  
> 24. Building A Keyboard-First Video Player with Svelte & Rust, [https://dev.to/lofifounder/building-a-keyboard-first-video-player-with-svelte-rust-dk7](https://dev.to/lofifounder/building-a-keyboard-first-video-player-with-svelte-rust-dk7)  
> 25. [dupe] Tauri 1.0 – Electron Alternative Powered by Rust - Hacker News, [https://news.ycombinator.com/item?id=31764015](https://news.ycombinator.com/item?id=31764015)  
> 26. Configuration - Tauri, [https://v2.tauri.app/reference/config/](https://v2.tauri.app/reference/config/)  
> 27. Rust GUI framework - Reddit, [https://www.reddit.com/r/rust/comments/1qq7n0n/rust_gui_framework/](https://www.reddit.com/r/rust/comments/1qq7n0n/rust_gui_framework/)  
> 28. Shottr Is the Ultimate Screenshot App for macOS Users; Here's Why, [https://beebom.com/shottr-ultimate-screenshot-app-macos-users/](https://beebom.com/shottr-ultimate-screenshot-app-macos-users/)  
> 29. Ask HN: Anyone making a living building desktop applications?, [https://news.ycombinator.com/item?id=30027925](https://news.ycombinator.com/item?id=30027925)  
> 30. Wayland: i3 to Sway migration - anarcat, [https://anarc.at/software/desktop/wayland/](https://anarc.at/software/desktop/wayland/)
