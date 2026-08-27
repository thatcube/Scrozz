//! Win32 arithmetic and bit composition, with no Win32 types in sight.
//!
//! Every rule that decides *what* Scrozz asks Windows to do — which extended
//! style bits a surface needs, which Z-order band it belongs in, where a
//! logical rectangle lands in device pixels, what an `HRESULT` actually means —
//! lives here, expressed in `u32`, `i32` and `f64`. The module compiles on
//! macOS and Linux as well as Windows, which is the entire point: it is
//! unit-tested by `cargo test` on the developer's Mac, where no Windows machine
//! exists to check the reasoning.
//!
//! The thin `windows` module beside it does nothing but hand these answers to
//! `user32`. It also carries `const` assertions proving that the mirrored
//! constants below are byte-for-byte the ones in the `windows` crate, so a
//! typo in a hex literal is a *compile* error under
//! `cargo check --target x86_64-pc-windows-msvc` rather than a silent
//! misbehaviour on a machine we cannot reach.
//!
//! # Why the style bits are a *specification*, not a one-shot write
//!
//! winit owns this window. When any winit-visible flag changes —
//! `set_cursor_hittest`, `set_window_level`, `set_visible` — winit recomputes
//! the **entire** extended style from its own `WindowFlags` and writes it
//! wholesale with `SetWindowLongPtrW(GWL_EXSTYLE, ...)`. Bits that winit does
//! not model, which includes both `WS_EX_NOACTIVATE` and `WS_EX_TOOLWINDOW`,
//! are erased in the process.
//!
//! That matters here because egui sends `ViewportCommand::MousePassthrough`
//! every frame, and eframe turns it into `set_cursor_hittest`. winit
//! early-returns while the value is unchanged, so the erasure does not happen
//! every frame — it happens the first time the pointer crosses a card edge,
//! which is precisely the moment the overlay is about to be clicked. A
//! one-shot write at window creation would therefore hold right up until the
//! instant it mattered, and then stop.
//!
//! So [`ExStyleSpec`] is a *predicate over the whole style word*
//! ([`ExStyleSpec::satisfied_by`]) with an idempotent repair
//! ([`ExStyleSpec::apply`]), and the native layer re-asserts it from a
//! `WM_STYLECHANGING` hook rather than trusting a single write.

use scrozz_core::{Error, LogicalPoint, LogicalRect, LogicalSize, ScaleFactor};

use crate::overlay::{OverlayBehavior, OverlayLevel};

// ---------------------------------------------------------------------------
// Extended window styles
// ---------------------------------------------------------------------------

/// `WS_EX_TOPMOST` — above every non-topmost window.
pub const WS_EX_TOPMOST: u32 = 0x0000_0008;
/// `WS_EX_TRANSPARENT` — hit-testing skips this window entirely.
pub const WS_EX_TRANSPARENT: u32 = 0x0000_0020;
/// `WS_EX_TOOLWINDOW` — no taskbar button, no Alt-Tab entry.
pub const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
/// `WS_EX_APPWINDOW` — forces a taskbar button; the bit to *clear*.
pub const WS_EX_APPWINDOW: u32 = 0x0004_0000;
/// `WS_EX_LAYERED` — composited by DWM with per-pixel alpha.
pub const WS_EX_LAYERED: u32 = 0x0008_0000;
/// `WS_EX_NOACTIVATE` — clicking does not activate the window or its app.
pub const WS_EX_NOACTIVATE: u32 = 0x0800_0000;

/// A requirement over a window's extended style word.
///
/// Two masks rather than one target value, because this window is shared with
/// winit: it is emphatically *not* our business what `WS_EX_WINDOWEDGE` or
/// `WS_EX_NOREDIRECTIONBITMAP` are doing. We assert only the bits Scrozz's
/// design depends on and leave the rest exactly as found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExStyleSpec {
    /// Bits that must be set.
    pub required: u32,
    /// Bits that must be clear.
    pub forbidden: u32,
}

impl ExStyleSpec {
    /// A specification requiring `required` and forbidding `forbidden`.
    ///
    /// A bit named in both is treated as forbidden, since the constructors
    /// below never do that and a contradiction should fail closed — an overlay
    /// that is too permissive steals focus, an overlay that is too restrictive
    /// merely looks wrong.
    #[must_use]
    pub const fn new(required: u32, forbidden: u32) -> Self {
        Self {
            required: required & !forbidden,
            forbidden,
        }
    }

    /// This specification with `bits` additionally required.
    #[must_use]
    pub const fn requiring(self, bits: u32) -> Self {
        Self::new(self.required | bits, self.forbidden & !bits)
    }

    /// This specification with `bits` additionally forbidden.
    #[must_use]
    pub const fn forbidding(self, bits: u32) -> Self {
        Self::new(self.required & !bits, self.forbidden | bits)
    }

    /// Whether a style word already satisfies this specification.
    ///
    /// Checked before every write so the repair is a no-op in the common case;
    /// `SetWindowLongPtrW(GWL_EXSTYLE, ...)` is not free — it can trigger a
    /// frame recalculation — and re-entering it from inside a style-change
    /// hook is how one writes an infinite loop in Win32.
    #[must_use]
    pub const fn satisfied_by(self, style: u32) -> bool {
        style & self.required == self.required && style & self.forbidden == 0
    }

    /// `style` corrected to satisfy this specification.
    ///
    /// Idempotent: `apply(apply(s)) == apply(s)` for every input, which is what
    /// makes it safe to call from a message hook that fires on the very change
    /// it is about to make.
    #[must_use]
    pub const fn apply(self, style: u32) -> u32 {
        (style | self.required) & !self.forbidden
    }
}

/// The extended-style requirement for an overlay surface.
///
/// The mapping, and the reasoning for each bit:
///
/// - **`WS_EX_TOOLWINDOW`, always.** D27 says the app is invisible at rest: no
///   taskbar entry, and no Alt-Tab entry either. `WS_EX_APPWINDOW` is the bit
///   that forces a taskbar button back on, so it is explicitly forbidden
///   rather than merely not requested — winit sets it from its own
///   `ON_TASKBAR` flag and would otherwise reinstate it.
/// - **`WS_EX_NOACTIVATE` iff the surface does not accept keys.** This is the
///   exact Windows analogue of an `NSPanel`'s `becomesKeyOnlyIfNeeded`. A
///   capture card is clicked to drag, copy or open — none of which need the
///   keyboard — so it must never pull the caret out of the user's editor. The
///   selection overlay is the opposite: it must read Escape, and keystrokes go
///   to the foreground window, so it cannot refuse activation.
/// - **`WS_EX_LAYERED` iff not opaque.** See [`layered_note`] for the part of
///   this that is a genuine judgement call.
/// - **`WS_EX_TOPMOST` for anything above [`OverlayLevel::Normal`].** Windows
///   has exactly two Z-order bands, so every level above normal collapses onto
///   the same one; see [`z_order`].
/// - **`WS_EX_TRANSPARENT` iff click-through.** All-or-nothing per window,
///   which is why it is toggled from the pointer position rather than set once.
///
/// Deliberately *not* mapped, and why: `join_all_spaces`, `stationary` and
/// `over_fullscreen` have no public Win32 equivalent. `IVirtualDesktopManager`
/// can ask which desktop a window is on and move it to another, but pinning —
/// the thing `NSWindowCollectionBehaviorCanJoinAllSpaces` does — is exposed
/// only through the undocumented `IVirtualDesktopPinnedApps` interface, whose
/// GUID changes between Windows builds. Scrozz will not ship a COM interface
/// that breaks on a Patch Tuesday. A topmost tool window already floats above
/// borderless-fullscreen apps, which covers the common case; exclusive
/// fullscreen still wins, on every platform.
#[must_use]
pub fn ex_style_spec(behavior: &OverlayBehavior) -> ExStyleSpec {
    let mut spec = ExStyleSpec::new(WS_EX_TOOLWINDOW, WS_EX_APPWINDOW);

    if !behavior.accepts_key {
        spec = spec.requiring(WS_EX_NOACTIVATE);
    } else {
        spec = spec.forbidding(WS_EX_NOACTIVATE);
    }

    if behavior.opaque {
        spec = spec.forbidding(WS_EX_LAYERED);
    } else {
        spec = spec.requiring(WS_EX_LAYERED);
    }

    if matches!(z_order(behavior.level), ZOrder::Topmost) {
        spec = spec.requiring(WS_EX_TOPMOST);
    }

    if behavior.click_through {
        spec = spec.requiring(WS_EX_TRANSPARENT);
    }

    spec
}

/// The subset of [`ex_style_spec`] that must survive winit's rewrites.
///
/// `WS_EX_TRANSPARENT` is excluded on purpose. That bit is genuinely winit's:
/// it is derived from `WindowFlags::IGNORE_CURSOR_EVENT`, which egui drives
/// every frame through `ViewportCommand::MousePassthrough`. Forcing it from a
/// style hook would fight the click-through logic and pin the overlay into
/// whichever state it happened to be in when the hook was installed.
///
/// `WS_EX_TOPMOST` is excluded for the same reason: winit models it as
/// `WindowFlags::ALWAYS_ON_TOP` and re-applies it through `SetWindowPos`, which
/// is the correct API for a Z-order change. Forcing the bit by hand would set
/// the flag without moving the window in the Z-order, which is exactly the
/// inconsistent state that makes topmost windows fall behind.
///
/// `WS_EX_LAYERED` is *not* volatile. Winit happens to add it while
/// passthrough is enabled, but removes it again when passthrough is disabled.
/// The capture card needs it in both states for per-pixel alpha, so the native
/// guard owns that bit just as it owns `NOACTIVATE` and `TOOLWINDOW`.
#[must_use]
pub fn enforced_ex_style_spec(behavior: &OverlayBehavior) -> ExStyleSpec {
    let full = ex_style_spec(behavior);
    let volatile = WS_EX_TRANSPARENT | WS_EX_TOPMOST;
    ExStyleSpec {
        required: full.required & !volatile,
        forbidden: full.forbidden & !volatile,
    }
}

/// Why the layered window is initialised with a fully opaque global multiplier.
///
/// A hidden window created with `WS_EX_LAYERED` is not guaranteed to become
/// visible until `SetLayeredWindowAttributes` or `UpdateLayeredWindow` has
/// initialised the layered path. The eframe hook runs before the window is
/// shown, so relying on winit to toggle the style later leaves a real invisible
/// startup path.
///
/// `LWA_ALPHA` with an alpha of 255 is an identity multiplier: it satisfies the
/// layered-window initialisation contract without making the window uniformly
/// translucent. DWM still composites the transparent client area winit created
/// with `DwmEnableBlurBehindWindow`, so rounded corners and shadows retain their
/// source alpha.
///
/// This interaction still needs a real Windows desktop run: cross-compilation
/// proves the API contract and types, not the DWM pixels.
pub const fn layered_note() -> &'static str {
    "WS_EX_LAYERED initialised with SetLayeredWindowAttributes(alpha=255); \
     DWM keeps compositing winit's transparent client area"
}

/// Whether native hit-testing should pass through this window.
///
/// Kept as a pure predicate so the `WM_NCHITTEST` hook and the style writer
/// cannot acquire subtly different meanings for `WS_EX_TRANSPARENT`.
#[must_use]
pub const fn hit_test_passes_through(ex_style: u32) -> bool {
    ex_style & WS_EX_TRANSPARENT != 0
}

// ---------------------------------------------------------------------------
// Z-order
// ---------------------------------------------------------------------------

/// Which of Windows' two Z-order bands a surface belongs in.
///
/// macOS has a continuum of window levels and Scrozz's [`OverlayLevel`] is
/// named after it. Windows has two: topmost and not. Everything Scrozz floats
/// therefore lands in the same band, and the ordering *within* it is by
/// activation, not by level.
///
/// The practical consequence is that [`OverlayLevel::Shielding`] does not
/// actually shield on Windows. There is no supported way to draw above the
/// taskbar's own topmost window, above UAC's secure desktop, or above an
/// exclusive-fullscreen game. The selection overlay covers the desktop and the
/// user's windows, which is what it is for, and does not cover the taskbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZOrder {
    /// `HWND_TOPMOST`.
    Topmost,
    /// `HWND_NOTOPMOST`.
    Normal,
}

/// The Z-order band for an overlay level.
#[must_use]
pub const fn z_order(level: OverlayLevel) -> ZOrder {
    match level {
        OverlayLevel::Normal => ZOrder::Normal,
        OverlayLevel::Floating
        | OverlayLevel::Status
        | OverlayLevel::AboveMenuBar
        | OverlayLevel::Shielding => ZOrder::Topmost,
    }
}

// ---------------------------------------------------------------------------
// Coordinates
// ---------------------------------------------------------------------------

/// The DPI Windows calls 100%.
pub const USER_DEFAULT_SCREEN_DPI: u32 = 96;

/// A rectangle in raw virtual-desktop device pixels.
///
/// Mirrors Win32 `RECT` — `right` and `bottom` exclusive — and every field is
/// `i32` rather than `u32` for one specific reason: **the virtual desktop's
/// origin is the primary monitor's top-left, so a monitor placed to the left of
/// or above it has negative coordinates**. A secondary display at `-1920` is
/// the ordinary two-monitor layout, not an edge case, and unsigned arithmetic
/// anywhere in this path puts the overlay 4 billion pixels off-screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceRect {
    /// Left edge, inclusive.
    pub left: i32,
    /// Top edge, inclusive.
    pub top: i32,
    /// Right edge, exclusive.
    pub right: i32,
    /// Bottom edge, exclusive.
    pub bottom: i32,
}

impl DeviceRect {
    /// A rectangle from Win32 `RECT` edges.
    #[must_use]
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Width in device pixels, clamped at zero for an inverted rectangle.
    #[must_use]
    pub const fn width(self) -> i32 {
        let w = self.right - self.left;
        if w < 0 { 0 } else { w }
    }

    /// Height in device pixels, clamped at zero for an inverted rectangle.
    #[must_use]
    pub const fn height(self) -> i32 {
        let h = self.bottom - self.top;
        if h < 0 { 0 } else { h }
    }

    /// Whether the rectangle encloses no pixels.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width() == 0 || self.height() == 0
    }

    /// Whether a point lies inside, treating right and bottom as exclusive.
    #[must_use]
    pub const fn contains(self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

/// Converts a Windows DPI value to a [`ScaleFactor`].
///
/// 96 is 100%, 120 is 125%, 144 is 150%; fractional scaling means the result
/// genuinely is not an integer. A zero DPI — what an out-parameter still holds
/// after a failed `GetDpiForMonitor` — falls back to 1.0 rather than dividing
/// by zero or panicking inside [`ScaleFactor::new`].
#[must_use]
pub fn scale_from_dpi(dpi: u32) -> ScaleFactor {
    if dpi == 0 {
        return ScaleFactor::IDENTITY;
    }
    let factor = f64::from(dpi) / f64::from(USER_DEFAULT_SCREEN_DPI);
    if factor.is_finite() && factor > 0.0 {
        ScaleFactor::new(factor)
    } else {
        ScaleFactor::IDENTITY
    }
}

/// A device rectangle in Scrozz's logical space.
///
/// Uses the same convention as the capture backend: **each monitor's logical
/// rectangle is its device rectangle divided by its own scale, origin
/// included**. There is no canonical global logical desktop on a mixed-DPI
/// Windows machine, and this convention is the one that round-trips exactly, so
/// an overlay anchored from a logical work area lands back on the device pixels
/// it came from.
#[must_use]
pub fn logical_from_device(rect: DeviceRect, scale: ScaleFactor) -> LogicalRect {
    let s = scale.get();
    LogicalRect::new(
        LogicalPoint::new(f64::from(rect.left) / s, f64::from(rect.top) / s),
        LogicalSize::new(f64::from(rect.width()) / s, f64::from(rect.height()) / s),
    )
}

/// A logical rectangle back in device pixels, as `SetWindowPos` wants it.
///
/// Rounds the origin and the size independently rather than rounding all four
/// edges, because `SetWindowPos` takes `(x, y, cx, cy)`: rounding edges would
/// let a card's width drift by a pixel between slots at fractional scale, which
/// reads as the stack shimmering as it grows.
///
/// Non-finite input collapses to an empty rectangle at the origin instead of
/// producing an `i32` from a `NaN` cast, whose value is unspecified in the
/// abstract and zero in practice — either way, not a window position.
#[must_use]
pub fn device_from_logical(rect: LogicalRect, scale: ScaleFactor) -> DeviceRect {
    let s = scale.get();
    let round = |v: f64| -> i32 {
        if v.is_finite() {
            v.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
        } else {
            0
        }
    };
    let left = round(rect.origin.x * s);
    let top = round(rect.origin.y * s);
    let width = round(rect.size.width * s).max(0);
    let height = round(rect.size.height * s).max(0);
    DeviceRect::new(
        left,
        top,
        left.saturating_add(width),
        top.saturating_add(height),
    )
}

/// Where the pointer is inside a window, in that window's logical points.
///
/// `GetCursorPos` reports the virtual desktop in device pixels; egui hit-tests
/// in logical points relative to the window's top-left. This is the conversion
/// between them, and it returns `None` when the pointer is outside the window
/// entirely — which is a meaningfully different answer from "at the edge",
/// because the overlay's click-through rule distinguishes *no pointer* from
/// *pointer over empty space*.
///
/// This exists because egui only knows where the pointer is once the pointer is
/// over a window that receives events, and a click-through overlay by
/// definition does not receive them. Without an external probe the overlay gets
/// stuck: transparent, therefore no events, therefore still transparent.
#[must_use]
pub fn pointer_in_window(
    cursor: (i32, i32),
    window: DeviceRect,
    scale: ScaleFactor,
) -> Option<(f64, f64)> {
    if window.is_empty() || !window.contains(cursor.0, cursor.1) {
        return None;
    }
    let s = scale.get();
    Some((
        f64::from(cursor.0 - window.left) / s,
        f64::from(cursor.1 - window.top) / s,
    ))
}

/// The monitor work area, as a logical rectangle for [`crate::OverlayWindow`].
///
/// Takes `rcWork` from `MONITORINFO` — the monitor minus the taskbar and any
/// appbars — never `rcMonitor`. Anchoring the capture stack to `rcMonitor`
/// tucks the bottom-left card underneath the taskbar, which is the default
/// taskbar position, so the bug would be immediate and universal.
#[must_use]
pub fn work_area_logical(rc_work: DeviceRect, dpi: u32) -> LogicalRect {
    logical_from_device(rc_work, scale_from_dpi(dpi))
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// `HRESULT` for `ERROR_INVALID_WINDOW_HANDLE` (1400).
pub const HR_INVALID_WINDOW_HANDLE: i32 = 0x8007_1400_u32 as i32;
/// `E_HANDLE` — `HRESULT` for `ERROR_INVALID_HANDLE` (6).
///
/// Distinct from [`HR_INVALID_WINDOW_HANDLE`], which is `ERROR_INVALID_WINDOW_HANDLE`
/// (1400). User32 sets the latter for a dead `HWND`, but COM and the WinRT
/// interop layer report a stale handle as the former, so both reach us and both
/// mean the same thing: the thing being addressed is gone.
pub const HR_E_HANDLE: i32 = 0x8007_0006_u32 as i32;
/// `HRESULT` for `ERROR_ACCESS_DENIED` (5).
pub const HR_ACCESS_DENIED: i32 = 0x8007_0005_u32 as i32;
/// `HRESULT` for `ERROR_INVALID_PARAMETER` (87).
pub const HR_INVALID_PARAMETER: i32 = 0x8007_0057_u32 as i32;
/// `E_INVALIDARG`.
pub const HR_E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
/// `E_OUTOFMEMORY`.
pub const HR_E_OUTOFMEMORY: i32 = 0x8007_000E_u32 as i32;
/// `E_NOINTERFACE`.
pub const HR_E_NOINTERFACE: i32 = 0x8000_4002_u32 as i32;
/// `E_NOTIMPL`.
pub const HR_E_NOTIMPL: i32 = 0x8000_4001_u32 as i32;
/// `RPC_E_CHANGED_MODE` — retry `RoInitialize` with the thread's existing model.
pub const HR_RPC_E_CHANGED_MODE: i32 = 0x8001_0106_u32 as i32;
/// `DV_E_FORMATETC` — the data object was asked for a format it cannot supply.
pub const HR_DV_E_FORMATETC: i32 = 0x8004_0064_u32 as i32;
/// `DV_E_TYMED` — the requested storage medium is not supported.
pub const HR_DV_E_TYMED: i32 = 0x8004_006C_u32 as i32;
/// `DRAGDROP_S_DROP` — a drag completed. A *success* code, despite the shape.
pub const HR_DRAGDROP_S_DROP: i32 = 0x0004_0100;
/// `DRAGDROP_S_CANCEL` — the user pressed Escape or released outside a target.
pub const HR_DRAGDROP_S_CANCEL: i32 = 0x0004_0101;
/// `CO_E_NOTINITIALIZED` — this thread never entered a COM/WinRT apartment.
///
/// The most consequential code in this file, because of how it usually
/// surfaces: not as a visible failure but as a *feature quietly not working*.
/// `GraphicsCaptureSession::IsSupported()` returns this on an uninitialised
/// thread, and code that writes `.unwrap_or(false)` reads it as "this machine
/// cannot do Windows.Graphics.Capture" and falls back to GDI — on a machine
/// that supports WGC perfectly well. The user gets a slower path with no
/// cursor control and no rounded-corner alpha, and nothing anywhere says why.
pub const HR_CO_E_NOTINITIALIZED: i32 = 0x8004_01F0_u32 as i32;
/// `S_FALSE` — succeeded, but the thing was already done.
pub const HR_S_FALSE: i32 = 1;

/// Whether an `HRESULT` indicates success.
///
/// `S_FALSE` (1) and the `DRAGDROP_S_*` codes are all non-zero successes, so
/// the test is the sign bit, never `== 0`. Getting this wrong turns a completed
/// drag into a reported failure.
#[must_use]
pub const fn hresult_is_ok(hr: i32) -> bool {
    hr >= 0
}

/// Maps an `HRESULT` to a typed, actionable [`Error`].
///
/// `context` names the operation in the user's terms — `"positioning the
/// capture overlay"`, not `"SetWindowPos"` — because these strings reach a
/// notification, and per D15 an error the user cannot act on is noise.
///
/// The classification that earns its keep is `ERROR_INVALID_WINDOW_HANDLE`.
/// Every overlay call races the window's own destruction: the user dismisses a
/// card while a frame is in flight, and the `HWND` stops existing between the
/// check and the call. That is [`Error::TargetGone`] — an expected outcome to
/// swallow — and reporting it as a platform failure would put a spurious error
/// on screen every time a card is dismissed.
#[must_use]
pub fn classify_hresult(hr: i32, context: &str) -> Error {
    match hr {
        HR_INVALID_WINDOW_HANDLE | HR_E_HANDLE => {
            Error::TargetGone(format!("{context}: the window no longer exists"))
        }
        HR_ACCESS_DENIED => Error::PermissionDenied {
            capability: context.to_string(),
            remedy: "Windows refused the operation; a window owned by a \
                     higher-integrity process cannot be modified from a \
                     standard-user app"
                .into(),
        },
        HR_INVALID_PARAMETER | HR_E_NOINTERFACE | HR_E_NOTIMPL => Error::InvalidRequest(format!(
            "{context}: Windows rejected the request (0x{hr:08X})"
        )),
        HR_E_OUTOFMEMORY => Error::Platform(format!("{context}: out of memory")),
        HR_RPC_E_CHANGED_MODE => Error::Platform(format!(
            "{context}: COM is already initialised in a different apartment on \
             this thread; retry RoInitialize with the matching model before \
             calling WinRT"
        )),
        HR_CO_E_NOTINITIALIZED => Error::Platform(format!(
            "{context}: this thread has not entered a COM apartment. Every \
             thread that touches WinRT must call RoInitialize first, and a \
             plain std::thread does not"
        )),
        HR_DV_E_FORMATETC | HR_DV_E_TYMED => Error::Unsupported {
            what: context.to_string(),
            why: "the drop target asked for a clipboard format or storage \
                  medium this drag does not offer"
                .into(),
        },
        _ if hresult_is_ok(hr) => Error::Platform(format!(
            "{context}: reported success (0x{hr:08X}) as a failure"
        )),
        _ => Error::Platform(format!("{context}: Windows error 0x{hr:08X}")),
    }
}

/// What happened when a thread tried to enter a COM/WinRT apartment.
///
/// `RPC_E_CHANGED_MODE` does not initialise WinRT. It says the requested model
/// conflicts with one already assigned to the thread, so the caller must retry
/// with the other model and may proceed only if that second call succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApartmentEntry {
    /// This call entered the apartment, and owes a matching uninitialise.
    ///
    /// Covers `S_FALSE` as well as `S_OK`: "already initialised with the same
    /// model" still increments the reference count, so it still owes one.
    Entered,
    /// Retry with the other apartment model.
    ///
    /// The failed call owns no reference and does not make WinRT usable. winit
    /// reaches this path after `OleInitialize` establishes its STA; retrying
    /// `RoInitialize(RO_INIT_SINGLETHREADED)` then performs the required WinRT
    /// initialisation and takes the reference Scrozz later balances.
    RetryOtherModel,
    /// The apartment could not be entered at all.
    Failed(i32),
}

impl ApartmentEntry {
    /// Whether WinRT calls may be made on this thread afterwards.
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        matches!(self, Self::Entered)
    }

    /// Whether this call owes a matching uninitialise.
    #[must_use]
    pub const fn owes_uninitialise(&self) -> bool {
        matches!(self, Self::Entered)
    }
}

/// Classifies the `HRESULT` from `RoInitialize` / `CoInitializeEx`.
#[must_use]
pub const fn classify_apartment_entry(hr: i32) -> ApartmentEntry {
    if hr == HR_RPC_E_CHANGED_MODE {
        ApartmentEntry::RetryOtherModel
    } else if hresult_is_ok(hr) {
        ApartmentEntry::Entered
    } else {
        ApartmentEntry::Failed(hr)
    }
}

/// Whether an `HRESULT` means "you forgot to enter an apartment".
///
/// Split out from [`classify_hresult`] because callers need to *branch* on it,
/// not merely report it: a WinRT probe that returns `false` for this reason is
/// answering a different question from one that returns `false` because the
/// feature is genuinely absent, and only the first is a bug in Scrozz.
#[must_use]
pub const fn is_uninitialised_apartment(hr: i32) -> bool {
    hr == HR_CO_E_NOTINITIALIZED
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> OverlayBehavior {
        OverlayBehavior::capture_card()
    }

    // -- style bits ------------------------------------------------------

    #[test]
    fn a_capture_card_never_activates_and_never_reaches_the_taskbar() {
        let spec = ex_style_spec(&card());
        assert_eq!(spec.required & WS_EX_NOACTIVATE, WS_EX_NOACTIVATE);
        assert_eq!(spec.required & WS_EX_TOOLWINDOW, WS_EX_TOOLWINDOW);
        assert_eq!(spec.forbidden & WS_EX_APPWINDOW, WS_EX_APPWINDOW);
    }

    #[test]
    fn a_transparent_card_is_layered_and_topmost() {
        let spec = ex_style_spec(&card());
        assert_eq!(spec.required & WS_EX_LAYERED, WS_EX_LAYERED);
        assert_eq!(spec.required & WS_EX_TOPMOST, WS_EX_TOPMOST);
    }

    #[test]
    fn the_selection_overlay_may_activate_because_it_reads_escape() {
        let spec = ex_style_spec(&OverlayBehavior::selection_overlay());
        assert_eq!(spec.required & WS_EX_NOACTIVATE, 0);
        assert_eq!(spec.forbidden & WS_EX_NOACTIVATE, WS_EX_NOACTIVATE);
    }

    #[test]
    fn click_through_adds_transparent_and_only_then() {
        assert_eq!(ex_style_spec(&card()).required & WS_EX_TRANSPARENT, 0);
        let mut through = card();
        through.click_through = true;
        assert_eq!(
            ex_style_spec(&through).required & WS_EX_TRANSPARENT,
            WS_EX_TRANSPARENT
        );
    }

    #[test]
    fn an_opaque_normal_window_is_neither_layered_nor_topmost() {
        let spec = ex_style_spec(&OverlayBehavior::default());
        assert_eq!(spec.required & WS_EX_TOPMOST, 0);
        assert_eq!(spec.forbidden & WS_EX_LAYERED, WS_EX_LAYERED);
    }

    #[test]
    fn apply_sets_required_and_clears_forbidden_and_leaves_the_rest_alone() {
        let spec = ex_style_spec(&card());
        // A style word carrying winit's own bits plus the taskbar bit.
        let foreign = 0x0000_0100 | 0x0002_0000;
        let before = foreign | WS_EX_APPWINDOW;
        let after = spec.apply(before);

        assert_eq!(after & WS_EX_APPWINDOW, 0, "taskbar bit must be cleared");
        assert_eq!(after & WS_EX_NOACTIVATE, WS_EX_NOACTIVATE);
        assert_eq!(after & foreign, foreign, "foreign bits must survive");
    }

    #[test]
    fn apply_is_idempotent_for_every_specification() {
        for behavior in [
            OverlayBehavior::default(),
            OverlayBehavior::capture_card(),
            OverlayBehavior::selection_overlay(),
        ] {
            let spec = ex_style_spec(&behavior);
            for style in [0u32, u32::MAX, 0x0C08_0080, WS_EX_APPWINDOW] {
                let once = spec.apply(style);
                assert_eq!(once, spec.apply(once), "not idempotent for {style:#x}");
                assert!(spec.satisfied_by(once));
            }
        }
    }

    #[test]
    fn satisfied_by_is_exactly_the_fixpoint_of_apply() {
        let spec = ex_style_spec(&card());
        for style in 0u32..512 {
            let style = style << 3;
            assert_eq!(
                spec.satisfied_by(style),
                spec.apply(style) == style,
                "disagreement at {style:#x}"
            );
        }
    }

    #[test]
    fn a_contradictory_specification_fails_closed_to_forbidden() {
        let spec = ExStyleSpec::new(WS_EX_NOACTIVATE, WS_EX_NOACTIVATE);
        assert_eq!(spec.required, 0);
        assert_eq!(spec.apply(WS_EX_NOACTIVATE), 0);
    }

    #[test]
    fn the_enforced_subset_leaves_winits_own_bits_to_winit() {
        let spec = enforced_ex_style_spec(&card());
        let volatile = WS_EX_TRANSPARENT | WS_EX_TOPMOST;
        assert_eq!(spec.required & volatile, 0);
        assert_eq!(spec.forbidden & volatile, 0);
        // LAYERED is restored too: winit models it only as a side effect of
        // passthrough, then removes it when the card becomes clickable.
        assert_eq!(
            spec.required,
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED
        );
        assert_eq!(spec.forbidden, WS_EX_APPWINDOW);
    }

    #[test]
    fn enforcing_repairs_the_style_winit_would_have_written() {
        // Exactly what winit's `to_window_styles` produces for a transparent,
        // always-on-top, click-through, taskbar-less window: no NOACTIVATE.
        let winit_wrote = WS_EX_TOPMOST | WS_EX_TRANSPARENT | WS_EX_LAYERED;
        let spec = enforced_ex_style_spec(&card());
        assert!(!spec.satisfied_by(winit_wrote));

        let repaired = spec.apply(winit_wrote);
        assert_eq!(repaired & WS_EX_NOACTIVATE, WS_EX_NOACTIVATE);
        assert_eq!(repaired & WS_EX_TOOLWINDOW, WS_EX_TOOLWINDOW);
        // winit's own bits are untouched.
        assert_eq!(repaired & WS_EX_TOPMOST, WS_EX_TOPMOST);
        assert_eq!(repaired & WS_EX_TRANSPARENT, WS_EX_TRANSPARENT);
        assert_eq!(repaired & WS_EX_LAYERED, WS_EX_LAYERED);
    }

    // -- z-order ---------------------------------------------------------

    #[test]
    fn every_floating_level_collapses_onto_the_single_topmost_band() {
        assert_eq!(z_order(OverlayLevel::Normal), ZOrder::Normal);
        for level in [
            OverlayLevel::Floating,
            OverlayLevel::Status,
            OverlayLevel::AboveMenuBar,
            OverlayLevel::Shielding,
        ] {
            assert_eq!(z_order(level), ZOrder::Topmost);
        }
    }

    // -- coordinates -----------------------------------------------------

    #[test]
    fn dpi_maps_to_the_scale_windows_means_by_it() {
        assert!((scale_from_dpi(96).get() - 1.0).abs() < 1e-12);
        assert!((scale_from_dpi(120).get() - 1.25).abs() < 1e-12);
        assert!((scale_from_dpi(144).get() - 1.5).abs() < 1e-12);
        assert!((scale_from_dpi(192).get() - 2.0).abs() < 1e-12);
    }

    #[test]
    fn a_failed_dpi_query_falls_back_to_one_rather_than_dividing_by_zero() {
        assert_eq!(scale_from_dpi(0).get(), ScaleFactor::IDENTITY.get());
    }

    #[test]
    fn a_monitor_left_of_the_primary_keeps_its_negative_origin() {
        // The ordinary two-monitor layout: 1920x1080 at 100%, placed left.
        let rect = DeviceRect::new(-1920, 0, 0, 1080);
        let logical = logical_from_device(rect, ScaleFactor::IDENTITY);
        assert_eq!(logical.origin.x, -1920.0);
        assert_eq!(logical.size.width, 1920.0);

        let back = device_from_logical(logical, ScaleFactor::IDENTITY);
        assert_eq!(back, rect, "round trip must preserve the negative origin");
    }

    #[test]
    fn a_monitor_above_the_primary_keeps_its_negative_top() {
        let rect = DeviceRect::new(0, -1080, 1920, 0);
        let round = device_from_logical(
            logical_from_device(rect, ScaleFactor::new(1.5)),
            ScaleFactor::new(1.5),
        );
        assert_eq!(round, rect);
    }

    #[test]
    fn scaled_monitors_round_trip_exactly() {
        for (dpi, rect) in [
            (96, DeviceRect::new(0, 0, 1920, 1040)),
            (120, DeviceRect::new(-2560, -300, 0, 1140)),
            (144, DeviceRect::new(1920, 0, 5760, 2160)),
            (192, DeviceRect::new(-3840, 0, 0, 2160)),
        ] {
            let scale = scale_from_dpi(dpi);
            let logical = logical_from_device(rect, scale);
            assert_eq!(device_from_logical(logical, scale), rect, "dpi {dpi}");
        }
    }

    #[test]
    fn a_non_finite_rectangle_does_not_become_a_window_position() {
        let bad = LogicalRect::new(
            LogicalPoint::new(f64::NAN, f64::INFINITY),
            LogicalSize::new(f64::NEG_INFINITY, 100.0),
        );
        let out = device_from_logical(bad, ScaleFactor::IDENTITY);
        assert_eq!(out.left, 0);
        assert_eq!(out.top, 0);
        assert_eq!(out.width(), 0);
    }

    #[test]
    fn the_work_area_is_the_taskbar_less_rectangle_in_points() {
        // 1920x1080 at 150% with a 48px taskbar at the bottom.
        let rc_work = DeviceRect::new(0, 0, 1920, 1032);
        let logical = work_area_logical(rc_work, 144);
        assert_eq!(logical.size.width, 1280.0);
        assert_eq!(logical.size.height, 688.0);
    }

    // -- hit testing -----------------------------------------------------

    #[test]
    fn a_pointer_outside_the_window_is_reported_as_absent() {
        let window = DeviceRect::new(100, 100, 300, 300);
        assert!(pointer_in_window((99, 200), window, ScaleFactor::IDENTITY).is_none());
        assert!(pointer_in_window((200, 99), window, ScaleFactor::IDENTITY).is_none());
        // Right and bottom are exclusive.
        assert!(pointer_in_window((300, 200), window, ScaleFactor::IDENTITY).is_none());
        assert!(pointer_in_window((200, 300), window, ScaleFactor::IDENTITY).is_none());
    }

    #[test]
    fn a_pointer_inside_becomes_window_local_points() {
        let window = DeviceRect::new(100, 100, 300, 300);
        let p = pointer_in_window((150, 220), window, ScaleFactor::IDENTITY).unwrap();
        assert_eq!(p, (50.0, 120.0));

        let scaled = pointer_in_window((250, 250), window, ScaleFactor::new(2.0)).unwrap();
        assert_eq!(scaled, (75.0, 75.0));
    }

    #[test]
    fn a_pointer_on_a_negatively_positioned_overlay_is_still_local() {
        let window = DeviceRect::new(-1920, -100, -920, 500);
        let p = pointer_in_window((-1900, -80), window, ScaleFactor::IDENTITY).unwrap();
        assert_eq!(p, (20.0, 20.0));
    }

    #[test]
    fn an_empty_window_never_reports_a_pointer() {
        let window = DeviceRect::new(10, 10, 10, 400);
        assert!(pointer_in_window((10, 20), window, ScaleFactor::IDENTITY).is_none());
    }

    // -- errors ----------------------------------------------------------

    #[test]
    fn a_destroyed_window_is_target_gone_not_a_platform_fault() {
        let err = classify_hresult(HR_INVALID_WINDOW_HANDLE, "anchoring the capture stack");
        assert!(matches!(err, Error::TargetGone(_)));
    }

    #[test]
    fn a_stale_handle_from_com_is_also_target_gone() {
        // User32 says ERROR_INVALID_WINDOW_HANDLE; COM says E_HANDLE. Mapping
        // only the first turns a dismissed card into a spurious platform error
        // on whichever path happened to go through COM.
        let err = classify_hresult(HR_E_HANDLE, "releasing the capture frame");
        assert!(
            matches!(err, Error::TargetGone(_)),
            "E_HANDLE should be TargetGone, got {err:?}"
        );
    }

    #[test]
    fn the_two_dead_handle_codes_are_genuinely_different_numbers() {
        // Guards against someone "tidying" the match arm by deleting one of
        // them on the assumption that they are the same constant twice.
        assert_ne!(HR_E_HANDLE, HR_INVALID_WINDOW_HANDLE);
        assert_eq!(HR_E_HANDLE, 0x8007_0006_u32 as i32);
        assert_eq!(HR_INVALID_WINDOW_HANDLE, 0x8007_1400_u32 as i32);
    }

    #[test]
    fn access_denied_is_reported_as_something_the_user_can_understand() {
        let err = classify_hresult(HR_ACCESS_DENIED, "anchoring the capture stack");
        assert!(err.is_actionable_by_user());
        assert!(err.to_string().contains("anchoring the capture stack"));
    }

    #[test]
    fn a_rejected_clipboard_format_is_unsupported_not_a_crash() {
        let err = classify_hresult(HR_DV_E_FORMATETC, "dragging a capture out");
        assert!(matches!(err, Error::Unsupported { .. }));
    }

    #[test]
    fn an_unknown_code_still_names_the_operation_and_the_number() {
        let err = classify_hresult(0x8007_1234_u32 as i32, "raising the overlay");
        let text = err.to_string();
        assert!(text.contains("raising the overlay"));
        assert!(text.contains("0x80071234"), "{text}");
    }

    #[test]
    fn drag_success_codes_are_successes() {
        assert!(hresult_is_ok(HR_DRAGDROP_S_DROP));
        assert!(hresult_is_ok(HR_DRAGDROP_S_CANCEL));
        assert!(hresult_is_ok(0), "S_OK");
        assert!(hresult_is_ok(1), "S_FALSE");
        assert!(!hresult_is_ok(HR_INVALID_WINDOW_HANDLE));
    }

    #[test]
    fn entering_an_apartment_owes_a_matching_exit() {
        // S_OK and S_FALSE both increment the reference count, so both owe an
        // uninitialise. Treating S_FALSE as "already done, nothing to undo" is
        // the classic leak.
        for hr in [0, HR_S_FALSE] {
            let entry = classify_apartment_entry(hr);
            assert_eq!(entry, ApartmentEntry::Entered, "0x{hr:08X}");
            assert!(entry.is_usable());
            assert!(entry.owes_uninitialise());
        }
    }

    #[test]
    fn a_changed_mode_requires_a_successful_retry() {
        // winit calls OleInitialize on the event-loop thread, so asking for an
        // MTA there returns this. COM has a model, but RoInitialize failed and
        // WinRT may not be used until a matching STA retry succeeds.
        let entry = classify_apartment_entry(HR_RPC_E_CHANGED_MODE);
        assert_eq!(entry, ApartmentEntry::RetryOtherModel);
        assert!(
            !entry.is_usable(),
            "the failed call did not initialise WinRT"
        );
        assert!(
            !entry.owes_uninitialise(),
            "a failed call took no reference"
        );
    }

    #[test]
    fn a_genuine_failure_is_neither_usable_nor_owed() {
        let entry = classify_apartment_entry(HR_E_OUTOFMEMORY);
        assert_eq!(entry, ApartmentEntry::Failed(HR_E_OUTOFMEMORY));
        assert!(!entry.is_usable());
        assert!(!entry.owes_uninitialise());
    }

    #[test]
    fn the_uninitialised_apartment_code_is_recognised() {
        // The whole point: this must be distinguishable from "unsupported", or
        // WGC silently degrades to GDI on hardware that supports it.
        assert!(is_uninitialised_apartment(HR_CO_E_NOTINITIALIZED));
        assert!(!is_uninitialised_apartment(HR_RPC_E_CHANGED_MODE));
        assert!(!is_uninitialised_apartment(0));
        assert!(!is_uninitialised_apartment(HR_E_NOTIMPL));
    }

    #[test]
    fn an_uninitialised_apartment_names_the_actual_mistake() {
        let err = classify_hresult(HR_CO_E_NOTINITIALIZED, "starting a screen capture");
        let text = err.to_string();
        assert!(text.contains("RoInitialize"), "{text}");
        assert!(text.contains("starting a screen capture"), "{text}");
    }
}
