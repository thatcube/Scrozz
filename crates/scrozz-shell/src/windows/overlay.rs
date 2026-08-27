//! Retrofits a winit/eframe `HWND` into a non-activating overlay window.
//!
//! eframe creates the window; this converts it, exactly as the macOS backend
//! converts an `NSWindow` into a non-activating `NSPanel`. Nothing here decides
//! anything — [`crate::win32`] holds every rule, tested on the host — so this
//! file is FFI, argument marshalling and one message hook.
//!
//! # The message hook, and why it is not optional
//!
//! winit recomputes the *entire* extended style from its own `WindowFlags`
//! whenever any of them changes, and writes it wholesale. `WS_EX_NOACTIVATE`
//! and `WS_EX_TOOLWINDOW` are not among winit's flags, so a plain
//! `SetWindowLongPtrW` at startup survives only until egui first toggles
//! click-through — which happens the instant the pointer crosses a capture
//! card, i.e. immediately before the user clicks it. The overlay would be
//! non-activating right up to the moment that mattered and then quietly stop
//! being so.
//!
//! `WM_STYLECHANGING` is the documented place to *veto* a style change: it
//! arrives before the write, carries a mutable `STYLESTRUCT`, and a subclass
//! may edit `styleNew`. Re-adding the required bits there makes the guarantee hold
//! against winit, against eframe, and against anything else that touches the
//! window.
//!
//! `WM_MOUSEACTIVATE` is handled in the same hook as a belt-and-braces second
//! line: returning `MA_NOACTIVATE` refuses activation on click even if the
//! style bit were somehow lost. The two mechanisms are independent, and the
//! cost of the second is four lines.
//!
//! # What is *not* verified here
//!
//! No line of this file has been executed on Windows. It type-checks against
//! `x86_64-pc-windows-msvc`, every constant is `const`-asserted against the
//! `windows` crate's own value, and every rule it applies is unit-tested — but
//! "it compiles and the maths is right" is not "it works", and this module says
//! so rather than implying otherwise.

use std::ffi::c_void;
use std::{cell::RefCell, mem::size_of};

use scrozz_core::{Error, LogicalRect, Result, ScaleFactor};
use windows::Win32::Foundation::{
    COLORREF, GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, SetLastError, WIN32_ERROR,
    WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetMonitorInfoW,
    HBITMAP, HDC, HGDIOBJ, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    SelectObject,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GWL_EXSTYLE, GWLP_WNDPROC, GetCursorPos, GetWindowLongPtrW, GetWindowRect,
    GetWindowThreadProcessId, HTTRANSPARENT, HWND_NOTOPMOST, HWND_TOPMOST, IsWindow, LWA_ALPHA,
    MA_NOACTIVATE, SHOW_WINDOW_CMD, STYLESTRUCT, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, ShowWindow, ULW_ALPHA,
    UpdateLayeredWindow, WINDOW_LONG_PTR_INDEX, WM_MOUSEACTIVATE, WM_NCDESTROY, WM_NCHITTEST,
    WM_STYLECHANGING, WNDPROC, WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT,
};

use crate::OverlayWindow;
use crate::overlay::{OverlayBehavior, OverlayLevel, OverlayReport};
use crate::win32::{
    DeviceRect, ExStyleSpec, ZOrder, classify_hresult, copy_premultiplied_rgba_to_bgra,
    device_from_logical, enforced_ex_style_spec, ex_style_spec, hit_test_passes_through,
    pointer_in_window, scale_from_dpi, work_area_logical, z_order,
};

// ---------------------------------------------------------------------------
// Proof that the mirrored constants are the real ones
// ---------------------------------------------------------------------------

// `crate::win32` restates these as bare `u32` so it can compile on macOS. That
// is only sound if the restatement is exact, and "I typed the hex correctly"
// is not a guarantee. These assertions turn a mistyped digit into a *compile*
// error under `cargo check --target x86_64-pc-windows-msvc`, which is a check
// the developer's Mac can actually run.
const _: () = assert!(crate::win32::WS_EX_TOPMOST == WS_EX_TOPMOST.0);
const _: () = assert!(crate::win32::WS_EX_TRANSPARENT == WS_EX_TRANSPARENT.0);
const _: () = assert!(crate::win32::WS_EX_TOOLWINDOW == WS_EX_TOOLWINDOW.0);
const _: () = assert!(crate::win32::WS_EX_APPWINDOW == WS_EX_APPWINDOW.0);
const _: () = assert!(crate::win32::WS_EX_LAYERED == WS_EX_LAYERED.0);
const _: () = assert!(crate::win32::WS_EX_NOACTIVATE == WS_EX_NOACTIVATE.0);
const _: () = assert!(crate::win32::USER_DEFAULT_SCREEN_DPI == 96);

// ---------------------------------------------------------------------------
// Thread affinity
// ---------------------------------------------------------------------------

/// Confirms the calling thread owns `hwnd`.
///
/// The Windows analogue of the macOS backend's `main_thread()` check, and it
/// exists for a sharper reason than symmetry. A window belongs to the thread
/// that created it; its message queue and its window procedure run there.
/// Installing a subclass from another thread would publish a `WNDPROC` that
/// gets invoked on the owning thread while its state lives in *this* thread's
/// thread-local storage — the hook would silently find nothing and do nothing.
///
/// `GetWindowThreadProcessId` returns 0 for an invalid handle, which can never
/// equal a real thread id, so a dead window fails this check too.
fn owning_thread(hwnd: HWND, context: &str) -> Result<()> {
    // SAFETY: both calls are pure queries that tolerate any handle value.
    let (owner, current) = unsafe { (GetWindowThreadProcessId(hwnd, None), GetCurrentThreadId()) };
    if owner == 0 {
        return Err(Error::TargetGone(format!(
            "{context}: the window handle is not a live window"
        )));
    }
    if owner != current {
        return Err(Error::Platform(format!(
            "{context}: called from thread {current} but the window belongs to \
             thread {owner}; overlay calls must run on the thread that owns the \
             event loop"
        )));
    }
    Ok(())
}

/// Makes the process per-monitor DPI aware v2, once.
///
/// Without it Windows lies: every monitor reports 96 DPI, coordinates come back
/// virtualised, and the result is bitmap-stretched. The overlay would be
/// positioned in a coordinate space that does not exist and drawn blurry.
///
/// Failure is deliberately ignored. The call fails with `ERROR_ACCESS_DENIED`
/// when awareness has already been set — by an application manifest, or by the
/// capture backend, which calls the same function — and in that case respecting
/// the existing setting is exactly right. It also fails on Windows 8.1 and
/// earlier, where the v2 context does not exist.
fn ensure_process_dpi_aware() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: no arguments to get wrong; the call is idempotent and its
        // failure modes are all benign.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    });
}

/// Turns a `windows` error into a typed Scrozz error.
fn map_err(err: windows::core::Error, context: &str) -> Error {
    classify_hresult(err.code().0, context)
}

/// Writes one window-long slot without losing a real Win32 failure behind the
/// API's ambiguous zero return value.
fn set_window_long_ptr(
    hwnd: HWND,
    index: WINDOW_LONG_PTR_INDEX,
    value: isize,
    context: &str,
) -> Result<isize> {
    // `SetWindowLongPtrW` returns the previous value, so zero can mean either
    // success or failure. Clearing last-error first is the documented way to
    // tell the two apart.
    let (previous, error) = unsafe {
        SetLastError(WIN32_ERROR(0));
        let previous = SetWindowLongPtrW(hwnd, index, value);
        (previous, GetLastError())
    };
    if previous == 0 && error.0 != 0 {
        return Err(Error::Platform(format!(
            "{context}: SetWindowLongPtrW failed with Win32 error {}",
            error.0
        )));
    }
    Ok(previous)
}

// ---------------------------------------------------------------------------
// The style guard
// ---------------------------------------------------------------------------

/// One guarded window's saved state.
struct Guard {
    /// The `HWND`, as an integer so the table needs no pointer types.
    hwnd: isize,
    /// The window procedure that was in place before ours.
    previous: WNDPROC,
    /// The bits to re-assert on every style change.
    spec: ExStyleSpec,
    /// Whether the window must refuse activation on click.
    refuse_activation: bool,
}

thread_local! {
    /// Guards installed on this thread.
    ///
    /// Thread-local rather than global because a window's procedure only ever
    /// runs on its owning thread, which [`owning_thread`] has already proved is
    /// this one. A `Vec` rather than a map because Scrozz has one overlay
    /// window, occasionally two, and a linear scan of two entries inside a
    /// message hook is cheaper than hashing.
    static GUARDS: RefCell<Vec<Guard>> = const { RefCell::new(Vec::new()) };
}

/// The subclass procedure that keeps the overlay's style bits alive.
///
/// # Safety
///
/// Invoked by Windows with a live `HWND` and the message's own parameters.
/// `WM_STYLECHANGING` documents `lparam` as a `STYLESTRUCT *` that the handler
/// may modify in place, which is the one dereference here.
unsafe extern "system" fn guard_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let key = hwnd.0 as isize;

    // Look the guard up and copy out what is needed *before* doing anything
    // that could re-enter, so the `RefCell` is never borrowed across a call
    // back into Windows.
    let found = GUARDS.with(|g| {
        g.borrow()
            .iter()
            .find(|entry| entry.hwnd == key)
            .map(|entry| (entry.previous, entry.spec, entry.refuse_activation))
    });

    let Some((previous, spec, refuse_activation)) = found else {
        // No guard: the only correct thing left is the system default. This is
        // unreachable in practice — the entry is removed only on WM_NCDESTROY,
        // after which no further messages arrive.
        // SAFETY: `DefWindowProcW` accepts any message for any live window.
        return unsafe {
            windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam)
        };
    };

    match msg {
        // The window's extended style is about to change. Anything that is not
        // ours passes through untouched; our required bits are put back.
        WM_STYLECHANGING if wparam.0 as i32 == GWL_EXSTYLE.0 => {
            let styles = lparam.0 as *mut STYLESTRUCT;
            if !styles.is_null() {
                // SAFETY: Windows documents `lparam` for this message as a
                // writable `STYLESTRUCT *` valid for the call's duration.
                unsafe {
                    (*styles).styleNew = spec.apply((*styles).styleNew);
                }
            }
        }
        // Refuse activation outright, independently of the style bit.
        WM_MOUSEACTIVATE if refuse_activation => {
            return LRESULT(MA_NOACTIVATE as isize);
        }
        // `WS_EX_TRANSPARENT` is what winit toggles for cursor hit-testing.
        // Returning HTTRANSPARENT as well makes the native hit-test answer
        // explicit instead of relying on style side effects alone.
        WM_NCHITTEST => {
            // SAFETY: reading a documented slot on the live window that invoked
            // this procedure.
            let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
            if hit_test_passes_through(style) {
                return LRESULT(HTTRANSPARENT as isize);
            }
        }
        // Last message a window ever receives: unhook and forget it.
        WM_NCDESTROY => {
            GUARDS.with(|g| g.borrow_mut().retain(|entry| entry.hwnd != key));
            // SAFETY: restoring the original procedure on a window that is
            // being destroyed; the value came from this window's own slot.
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wndproc_to_isize(previous));
            }
        }
        _ => {}
    }

    // SAFETY: `previous` is the procedure this window had before the subclass
    // was installed, and chaining to it is the required behaviour.
    unsafe { CallWindowProcW(previous, hwnd, msg, wparam, lparam) }
}

/// Reinterprets a `WNDPROC` as the integer `SetWindowLongPtrW` speaks.
fn wndproc_to_isize(proc: WNDPROC) -> isize {
    match proc {
        Some(f) => f as usize as isize,
        None => 0,
    }
}

/// Reinterprets `SetWindowLongPtrW`'s return value as a `WNDPROC`.
///
/// # Safety
///
/// `value` must be zero or a genuine window-procedure address, which is what
/// `SetWindowLongPtrW(GWLP_WNDPROC, ..)` returns.
unsafe fn isize_to_wndproc(value: isize) -> WNDPROC {
    if value == 0 {
        None
    } else {
        // SAFETY: the caller guarantees `value` is a window-procedure address,
        // and `WNDPROC` is `Option<fn(..)>`, whose non-null representation is
        // the function pointer itself.
        Some(unsafe {
            std::mem::transmute::<
                isize,
                unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
            >(value)
        })
    }
}

// ---------------------------------------------------------------------------
// The overlay
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presentation {
    CompositedClient,
    LayeredBitmap,
}

impl Presentation {
    const fn detail(self) -> &'static str {
        match self {
            Self::CompositedClient => "layered client-composition path initialized",
            Self::LayeredBitmap => {
                "per-pixel UpdateLayeredWindow path selected (initialized before show)"
            }
        }
    }
}

/// A winit window retrofitted into a Scrozz overlay.
///
/// Holds a borrowed `HWND`: eframe owns the window and outlives this. Dropping
/// a `WindowsOverlay` leaves the guard installed, which is correct — the guard
/// must keep working for as long as the window exists, and it removes itself on
/// `WM_NCDESTROY`.
#[derive(Debug)]
pub struct WindowsOverlay {
    hwnd: HWND,
    non_activating: bool,
    guarded: bool,
}

impl WindowsOverlay {
    /// Adopts a native window handle.
    ///
    /// This is the path from `raw-window-handle`: eframe reports
    /// `RawWindowHandle::Win32`, whose `hwnd` field is the window itself —
    /// unlike AppKit, where the handle is a *view* and its window has to be
    /// asked for.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a null handle, [`Error::TargetGone`] if
    /// the handle is not a live window, and [`Error::Platform`] if the calling
    /// thread does not own it.
    ///
    /// # Safety
    ///
    /// `handle` must be a live `HWND`, as obtained from a
    /// `RawWindowHandle::Win32` that has not been dropped.
    pub unsafe fn adopt(handle: *mut c_void) -> Result<Self> {
        if handle.is_null() {
            return Err(Error::InvalidRequest(
                "null HWND passed to WindowsOverlay::adopt".to_owned(),
            ));
        }
        let hwnd = HWND(handle);
        // SAFETY: `IsWindow` is documented to accept any handle value,
        // including a stale one, and to answer without dereferencing it.
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Err(Error::TargetGone(
                "the HWND passed to WindowsOverlay::adopt is not a live window".to_owned(),
            ));
        }
        owning_thread(hwnd, "adopting an overlay window")?;
        ensure_process_dpi_aware();
        Ok(Self {
            hwnd,
            non_activating: false,
            guarded: false,
        })
    }

    /// The adopted window handle.
    #[must_use]
    pub const fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Applies overlay properties, and installs the guard that keeps them.
    ///
    /// Order matters. The guard goes in *first*, so that the style write which
    /// follows is itself seen and corrected by the hook, and so that a window
    /// which for any reason ends up with a partly-applied style is still
    /// repaired on the next change rather than left half-configured.
    ///
    /// # Errors
    ///
    /// [`Error::Platform`] if called off the owning thread;
    /// [`Error::TargetGone`] if the window died in the meantime.
    pub fn apply(&mut self, behavior: &OverlayBehavior) -> Result<OverlayReport> {
        self.apply_with_presentation(behavior, Presentation::CompositedClient)
    }

    /// Applies overlay properties for pixels submitted by
    /// [`LayeredPresenter`].
    ///
    /// Unlike [`Self::apply`], this deliberately does not call
    /// `SetLayeredWindowAttributes`: Microsoft documents that doing so prevents
    /// a later `UpdateLayeredWindow` until `WS_EX_LAYERED` is cleared and set
    /// again. The presenter supplies the first transparent bitmap before the
    /// hidden window is shown, so there is no uninitialized visible interval.
    ///
    /// # Errors
    ///
    /// As [`Self::apply`].
    pub fn apply_layered_bitmap(&mut self, behavior: &OverlayBehavior) -> Result<OverlayReport> {
        self.apply_with_presentation(behavior, Presentation::LayeredBitmap)
    }

    fn apply_with_presentation(
        &mut self,
        behavior: &OverlayBehavior,
        presentation: Presentation,
    ) -> Result<OverlayReport> {
        owning_thread(self.hwnd, "configuring an overlay window")?;

        let spec = ex_style_spec(behavior);
        let enforced = enforced_ex_style_spec(behavior);
        let refuse_activation = !behavior.accepts_key;

        self.install_guard(enforced, refuse_activation)?;

        // SAFETY: live window on the owning thread; `GWL_EXSTYLE` is a read of
        // a documented slot.
        let current = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) } as u32;
        let wanted = spec.apply(current);
        if wanted != current {
            set_window_long_ptr(
                self.hwnd,
                GWL_EXSTYLE,
                wanted as isize,
                "configuring the overlay's extended styles",
            )?;
        }

        if spec.required & WS_EX_LAYERED.0 != 0 && presentation == Presentation::CompositedClient {
            // SAFETY: the style was just applied to this live top-level window.
            // Alpha 255 is an identity global multiplier; see `layered_note`.
            unsafe { SetLayeredWindowAttributes(self.hwnd, COLORREF(0), 255, LWA_ALPHA) }
                .map_err(|e| map_err(e, "initialising the layered overlay window"))?;
        }

        // The Z-order band has to be *moved*, not merely flagged: setting
        // WS_EX_TOPMOST without a SetWindowPos leaves the bit on a window that
        // is still ordered normally, which is how "always on top" windows end
        // up behind things.
        self.set_z_order(z_order(behavior.level))?;

        // SAFETY: reading back the slot just written.
        let after = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) } as u32;
        if !spec.satisfied_by(after) {
            return Err(Error::Platform(format!(
                "the overlay's extended styles did not stick: got 0x{after:08X}, \
                 require 0x{:08X}, forbid 0x{:08X}",
                spec.required, spec.forbidden
            )));
        }
        self.non_activating = !refuse_activation || after & WS_EX_NOACTIVATE.0 != 0;

        let detail = if self.non_activating {
            format!(
                "HWND {:p}: ex-style 0x{after:08X}{}, {}, \
                 style guard installed on WM_STYLECHANGING and WM_NCHITTEST{}",
                self.hwnd.0,
                if refuse_activation {
                    " with WS_EX_NOACTIVATE"
                } else {
                    " (activation permitted: this surface reads the keyboard)"
                },
                presentation.detail(),
                if refuse_activation {
                    " and WM_MOUSEACTIVATE"
                } else {
                    ""
                },
            )
        } else {
            format!(
                "HWND {:p}: WS_EX_NOACTIVATE did not stick (ex-style \
                 0x{after:08X}); clicking the overlay will pull focus to Scrozz",
                self.hwnd.0,
            )
        };

        Ok(OverlayReport {
            non_activating: self.non_activating,
            detail,
        })
    }

    /// Moves the window into a Z-order band without activating it.
    ///
    /// # Errors
    ///
    /// Whatever `SetWindowPos` reported, classified.
    pub fn set_z_order(&mut self, order: ZOrder) -> Result<()> {
        let insert_after = match order {
            ZOrder::Topmost => HWND_TOPMOST,
            ZOrder::Normal => HWND_NOTOPMOST,
        };
        // SAFETY: live window on the owning thread. NOMOVE|NOSIZE make the
        // position and size arguments ignored, so the zeros are inert.
        unsafe {
            SetWindowPos(
                self.hwnd,
                Some(insert_after),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        }
        .map_err(|e| map_err(e, "raising the capture overlay"))
    }

    /// Sets the overlay's stacking level.
    ///
    /// # Errors
    ///
    /// As [`Self::set_z_order`].
    pub fn set_level(&mut self, level: OverlayLevel) -> Result<()> {
        owning_thread(self.hwnd, "setting an overlay window level")?;
        self.set_z_order(z_order(level))
    }

    /// Shows the window without giving it focus.
    ///
    /// `SW_SHOWNOACTIVATE` rather than `SW_SHOW`, because the whole point of
    /// the surface is that it appears while the user keeps typing somewhere
    /// else.
    ///
    /// # Errors
    ///
    /// [`Error::Platform`] if called off the owning thread.
    pub fn show_without_activating(&mut self) -> Result<()> {
        owning_thread(self.hwnd, "showing an overlay window")?;
        // SAFETY: live window on the owning thread. `ShowWindow` returns the
        // previous visibility, not a status, so there is nothing to check.
        unsafe {
            let _ = ShowWindow(self.hwnd, SHOW_WINDOW_CMD(SW_SHOWNOACTIVATE.0));
        }
        Ok(())
    }

    /// Everything worth asserting about the window's current native state.
    ///
    /// Exists for the same reason the macOS backend has one: the non-activating
    /// property is the guarantee the whole design rests on, and it should be
    /// *provable* on a real machine rather than assumed. The Windows smoke
    /// test prints this.
    ///
    /// # Errors
    ///
    /// [`Error::TargetGone`] if the window has been destroyed.
    pub fn diagnostics(&self) -> Result<OverlayDiagnostics> {
        // SAFETY: `IsWindow` tolerates stale handles by contract.
        if !unsafe { IsWindow(Some(self.hwnd)) }.as_bool() {
            return Err(Error::TargetGone(
                "inspecting an overlay window: it has been destroyed".to_owned(),
            ));
        }
        // SAFETY: live window; reading a documented slot.
        let ex_style = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) } as u32;
        Ok(OverlayDiagnostics {
            ex_style,
            no_activate: ex_style & WS_EX_NOACTIVATE.0 != 0,
            tool_window: ex_style & WS_EX_TOOLWINDOW.0 != 0,
            app_window: ex_style & WS_EX_APPWINDOW.0 != 0,
            layered: ex_style & WS_EX_LAYERED.0 != 0,
            topmost: ex_style & WS_EX_TOPMOST.0 != 0,
            click_through: ex_style & WS_EX_TRANSPARENT.0 != 0,
            guarded: self.guarded,
            window_rect: self.window_rect().unwrap_or_default(),
            scale: self.scale(),
        })
    }

    /// The window's outer rectangle in virtual-desktop device pixels.
    ///
    /// # Errors
    ///
    /// Whatever `GetWindowRect` reported, classified.
    pub fn window_rect(&self) -> Result<DeviceRect> {
        let mut rect = RECT::default();
        // SAFETY: `rect` is a live, correctly-sized out-parameter.
        unsafe { GetWindowRect(self.hwnd, &raw mut rect) }
            .map_err(|e| map_err(e, "reading the overlay's position"))?;
        Ok(DeviceRect::new(
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
        ))
    }

    /// The scale factor of the monitor the window is predominantly on.
    ///
    /// Re-read on every use rather than cached: a window dragged between a 150%
    /// laptop panel and a 100% external monitor changes scale mid-session, and
    /// a stale factor puts every subsequent frame in the wrong place.
    #[must_use]
    pub fn scale(&self) -> ScaleFactor {
        ensure_process_dpi_aware();
        // SAFETY: `MonitorFromWindow` with DEFAULTTONEAREST never fails and
        // never returns null for a live window.
        let monitor = unsafe { MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST) };
        scale_from_dpi(dpi_for_monitor(monitor))
    }

    /// The work area of the monitor this window sits on, in logical points.
    ///
    /// `rcWork`, never `rcMonitor`: anchoring the capture stack to the raw
    /// monitor rectangle tucks the bottom-left card under the taskbar, which is
    /// where the taskbar lives by default.
    ///
    /// # Errors
    ///
    /// [`Error::Platform`] if `GetMonitorInfoW` refused, which happens when the
    /// monitor was unplugged between two frames.
    pub fn work_area(&self) -> Result<LogicalRect> {
        ensure_process_dpi_aware();
        // SAFETY: as in `scale`.
        let monitor = unsafe { MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST) };
        monitor_work_area(monitor)
    }

    /// Where the pointer is inside this window, in logical points.
    ///
    /// `None` when the pointer is elsewhere, which is a different answer from
    /// "unknown" and the overlay's click-through rule depends on the
    /// difference. This exists because a click-through window receives no mouse
    /// messages at all, so egui cannot see the pointer return — without an
    /// external probe the overlay latches transparent forever.
    #[must_use]
    pub fn pointer(&self) -> Option<(f64, f64)> {
        let mut point = POINT::default();
        // SAFETY: `point` is a live, correctly-sized out-parameter.
        unsafe { GetCursorPos(&raw mut point) }.ok()?;
        let rect = self.window_rect().ok()?;
        pointer_in_window((point.x, point.y), rect, self.scale())
    }

    /// Installs the style guard, replacing any guard already on this window.
    fn install_guard(&mut self, spec: ExStyleSpec, refuse_activation: bool) -> Result<()> {
        let key = self.hwnd.0 as isize;

        // Already guarded: update the specification in place. Re-subclassing
        // would chain the guard to itself and recurse forever.
        let updated = GUARDS.with(|g| {
            let mut guards = g.borrow_mut();
            if let Some(entry) = guards.iter_mut().find(|entry| entry.hwnd == key) {
                entry.spec = spec;
                entry.refuse_activation = refuse_activation;
                true
            } else {
                false
            }
        });
        if updated {
            self.guarded = true;
            return Ok(());
        }

        let raw = set_window_long_ptr(
            self.hwnd,
            GWLP_WNDPROC,
            wndproc_to_isize(Some(guard_proc)),
            "installing the overlay window procedure",
        )?;
        // SAFETY: `raw` is the previous procedure returned from this live
        // window's `GWLP_WNDPROC` slot.
        let previous = unsafe { isize_to_wndproc(raw) };
        if previous.is_none() {
            return Err(Error::Platform(
                "installing the overlay window procedure returned no previous procedure".to_owned(),
            ));
        }

        GUARDS.with(|g| {
            g.borrow_mut().push(Guard {
                hwnd: key,
                previous,
                spec,
                refuse_activation,
            });
        });
        self.guarded = true;
        Ok(())
    }
}

/// CPU-backed presenter for a per-pixel-alpha layered window.
///
/// A normal DXGI swap chain attached to an `HWND` may expose only
/// `DXGI_ALPHA_MODE_IGNORE`; that is the case for WARP on Windows ARM VMs and
/// turns a transparent full-screen overlay into a black sheet. This path avoids
/// swap-chain alpha entirely: egui's RGBA frame is copied into a persistent
/// top-down DIB and handed to DWM through `UpdateLayeredWindow`.
pub struct LayeredPresenter {
    hwnd: HWND,
    dib: Option<MemoryDib>,
}

impl std::fmt::Debug for LayeredPresenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayeredPresenter")
            .field("hwnd", &self.hwnd.0)
            .field(
                "size",
                &self.dib.as_ref().map(|dib| (dib.width, dib.height)),
            )
            .finish()
    }
}

impl LayeredPresenter {
    /// Binds a presenter to a live window on its owning thread.
    ///
    /// # Errors
    ///
    /// Returns a target or thread-affinity error for an invalid/off-thread
    /// handle. The window must already carry `WS_EX_LAYERED`; the first
    /// presentation initializes that path.
    pub fn new(hwnd: isize) -> Result<Self> {
        if hwnd == 0 {
            return Err(Error::InvalidRequest(
                "null HWND passed to LayeredPresenter::new".to_owned(),
            ));
        }
        let hwnd = HWND(hwnd as *mut c_void);
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Err(Error::TargetGone(
                "the HWND passed to LayeredPresenter::new is not a live window".to_owned(),
            ));
        }
        owning_thread(hwnd, "creating a layered-window presenter")?;
        let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
        if style & WS_EX_LAYERED.0 == 0 {
            return Err(Error::Platform(format!(
                "creating a layered-window presenter: HWND {:p} lacks WS_EX_LAYERED \
                 (ex-style 0x{style:08X})",
                hwnd.0
            )));
        }
        Ok(Self { hwnd, dib: None })
    }

    /// Initializes or clears the whole window to transparent pixels.
    ///
    /// Call this before making a newly-created window visible so an unpainted
    /// black client area can never flash on screen.
    ///
    /// # Errors
    ///
    /// Returns an explicit allocation or `UpdateLayeredWindow` error.
    pub fn present_transparent(&mut self, width: u32, height: u32) -> Result<()> {
        let hwnd = self.hwnd;
        let dib = self.dib(width, height)?;
        dib.bytes_mut()?.fill(0);
        present_dib(hwnd, dib)
    }

    /// Presents one premultiplied-alpha RGBA frame.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request error when the buffer dimensions disagree,
    /// or a native error when the DIB/presentation path fails.
    pub fn present_premultiplied_rgba(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<()> {
        let expected = buffer_len(width, height)?;
        if rgba.len() != expected {
            return Err(Error::InvalidRequest(format!(
                "presenting a {width}x{height} layered frame needs {expected} RGBA bytes, got {}",
                rgba.len()
            )));
        }

        let hwnd = self.hwnd;
        let dib = self.dib(width, height)?;
        copy_premultiplied_rgba_to_bgra(rgba, dib.bytes_mut()?)?;
        present_dib(hwnd, dib)
    }

    fn dib(&mut self, width: u32, height: u32) -> Result<&mut MemoryDib> {
        let replace = self
            .dib
            .as_ref()
            .is_none_or(|dib| dib.width != width || dib.height != height);
        if replace {
            self.dib = Some(MemoryDib::create(width, height)?);
        }
        self.dib.as_mut().ok_or_else(|| {
            Error::Platform("the layered-window DIB was not retained after creation".to_owned())
        })
    }
}

struct MemoryDib {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    width: u32,
    height: u32,
}

impl MemoryDib {
    fn create(width: u32, height: u32) -> Result<Self> {
        let _ = buffer_len(width, height)?;
        let width_i32 = i32::try_from(width).map_err(|_| {
            Error::InvalidRequest(format!("layered-window width {width} exceeds Win32 i32"))
        })?;
        let height_i32 = i32::try_from(height).map_err(|_| {
            Error::InvalidRequest(format!("layered-window height {height} exceeds Win32 i32"))
        })?;

        let dc = unsafe { CreateCompatibleDC(None) };
        if dc.is_invalid() {
            return Err(Error::Platform(
                "creating the layered-window memory DC failed".to_owned(),
            ));
        }

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or(40),
                biWidth: width_i32,
                biHeight: -height_i32,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = core::ptr::null_mut();
        let bitmap = unsafe {
            CreateDIBSection(
                Some(dc),
                &raw const info,
                DIB_RGB_COLORS,
                &raw mut bits,
                None,
                0,
            )
        };
        let bitmap = match bitmap {
            Ok(bitmap) if !bitmap.is_invalid() && !bits.is_null() => bitmap,
            _ => {
                let _ = unsafe { DeleteDC(dc) };
                return Err(Error::Platform(
                    "creating the layered-window 32-bit DIB failed".to_owned(),
                ));
            }
        };
        let previous = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
        if previous.is_invalid() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
                let _ = DeleteDC(dc);
            }
            return Err(Error::Platform(
                "selecting the layered-window DIB into its memory DC failed".to_owned(),
            ));
        }

        Ok(Self {
            dc,
            bitmap,
            previous,
            bits: bits.cast(),
            width,
            height,
        })
    }

    fn bytes_mut(&mut self) -> Result<&mut [u8]> {
        let len = buffer_len(self.width, self.height)?;
        if self.bits.is_null() {
            return Err(Error::Platform(
                "the layered-window DIB lost its pixel address".to_owned(),
            ));
        }
        Ok(unsafe { core::slice::from_raw_parts_mut(self.bits, len) })
    }
}

impl Drop for MemoryDib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

fn buffer_len(width: u32, height: u32) -> Result<usize> {
    if width == 0 || height == 0 {
        return Err(Error::InvalidRequest(
            "cannot present a zero-sized layered window".to_owned(),
        ));
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| {
            Error::InvalidRequest(format!(
                "layered-window dimensions {width}x{height} overflow addressable memory"
            ))
        })
}

fn present_dib(hwnd: HWND, dib: &MemoryDib) -> Result<()> {
    owning_thread(hwnd, "presenting a layered-window frame")?;
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Err(Error::TargetGone(
            "presenting a layered-window frame: the window was destroyed".to_owned(),
        ));
    }

    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &raw mut rect) }
        .map_err(|error| map_err(error, "reading the layered window position"))?;
    let destination = POINT {
        x: rect.left,
        y: rect.top,
    };
    let size = SIZE {
        cx: i32::try_from(dib.width).map_err(|_| {
            Error::InvalidRequest(format!(
                "layered-window width {} exceeds Win32 i32",
                dib.width
            ))
        })?,
        cy: i32::try_from(dib.height).map_err(|_| {
            Error::InvalidRequest(format!(
                "layered-window height {} exceeds Win32 i32",
                dib.height
            ))
        })?,
    };
    let source = POINT::default();
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    unsafe {
        UpdateLayeredWindow(
            hwnd,
            None,
            Some(&raw const destination),
            Some(&raw const size),
            Some(dib.dc),
            Some(&raw const source),
            COLORREF(0),
            Some(&raw const blend),
            ULW_ALPHA,
        )
    }
    .map_err(|error| {
        map_err(
            error,
            "submitting premultiplied pixels to UpdateLayeredWindow",
        )
    })
}

impl OverlayWindow for WindowsOverlay {
    fn set_frame(&mut self, frame: LogicalRect) -> Result<()> {
        owning_thread(self.hwnd, "positioning an overlay window")?;
        let rect = device_from_logical(frame, self.scale());
        // SAFETY: live window on the owning thread. `HWND_TOPMOST` keeps the
        // move and the Z-order in one call; SWP_NOACTIVATE is what stops a
        // reposition from stealing focus, which is the whole point.
        unsafe {
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                rect.width(),
                rect.height(),
                SWP_NOACTIVATE,
            )
        }
        .map_err(|e| map_err(e, "anchoring the capture overlay"))
    }

    fn set_click_through(&mut self, passthrough: bool) -> Result<()> {
        owning_thread(self.hwnd, "setting overlay click-through")?;
        // SAFETY: live window on the owning thread; documented slot.
        let current = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) } as u32;
        let wanted = if passthrough {
            current | WS_EX_TRANSPARENT.0 | WS_EX_LAYERED.0
        } else {
            current & !WS_EX_TRANSPARENT.0
        };
        if wanted != current {
            set_window_long_ptr(
                self.hwnd,
                GWL_EXSTYLE,
                wanted as isize,
                "changing overlay hit-testing",
            )?;
        }
        // SAFETY: live window on its owning thread; documented slot.
        let after = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) } as u32;
        if hit_test_passes_through(after) != passthrough || after & WS_EX_LAYERED.0 == 0 {
            return Err(Error::Platform(format!(
                "overlay hit-testing did not stick: passthrough={passthrough}, \
                 ex-style=0x{after:08X}"
            )));
        }
        Ok(())
    }
}

/// A snapshot of an overlay window's native state.
///
/// Every field is a fact read back from Windows, not a record of what was
/// requested. The distinction is the point: a smoke test that printed the
/// intent would pass on a machine where none of it took effect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayDiagnostics {
    /// The raw extended style word.
    pub ex_style: u32,
    /// `WS_EX_NOACTIVATE` — clicking does not activate Scrozz.
    pub no_activate: bool,
    /// `WS_EX_TOOLWINDOW` — no taskbar button, no Alt-Tab entry.
    pub tool_window: bool,
    /// `WS_EX_APPWINDOW` — a taskbar button. Should always be `false`.
    pub app_window: bool,
    /// `WS_EX_LAYERED` — composited with alpha.
    pub layered: bool,
    /// `WS_EX_TOPMOST` — above ordinary windows.
    pub topmost: bool,
    /// `WS_EX_TRANSPARENT` — clicks currently pass through.
    pub click_through: bool,
    /// Whether the `WM_STYLECHANGING` guard is installed.
    pub guarded: bool,
    /// Outer rectangle in virtual-desktop device pixels.
    pub window_rect: DeviceRect,
    /// Scale factor of the monitor the window is on.
    pub scale: ScaleFactor,
}

impl OverlayDiagnostics {
    /// Whether the window is configured the way an invisible-at-rest overlay
    /// must be: never activating, never in the taskbar, always on top.
    #[must_use]
    pub const fn is_well_formed_overlay(&self) -> bool {
        self.no_activate
            && self.tool_window
            && !self.app_window
            && self.layered
            && self.topmost
            && self.guarded
    }
}

// ---------------------------------------------------------------------------
// Free functions, for callers with a monitor but no overlay yet
// ---------------------------------------------------------------------------

/// The effective DPI of a monitor, falling back to 96.
///
/// A failed query means the monitor was unplugged between enumeration and this
/// call. A wrong scale is better than no work area at all, and 96 is the value
/// Windows itself uses when it does not know.
#[must_use]
pub fn dpi_for_monitor(monitor: HMONITOR) -> u32 {
    ensure_process_dpi_aware();
    let mut x = 0u32;
    let mut y = 0u32;
    // SAFETY: both out-parameters are live and correctly typed.
    let result = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut x, &raw mut y) };
    if result.is_err() || x == 0 {
        return crate::win32::USER_DEFAULT_SCREEN_DPI;
    }
    // Windows reports separate horizontal and vertical DPI but has never
    // shipped a configuration where they differ for MDT_EFFECTIVE_DPI. The
    // horizontal value is authoritative; averaging would produce a scale
    // matching neither axis.
    x
}

/// The work area of a monitor, in Scrozz's logical points.
///
/// # Errors
///
/// [`Error::Platform`] if `GetMonitorInfoW` refused.
pub fn monitor_work_area(monitor: HMONITOR) -> Result<LogicalRect> {
    let mut info = MONITORINFO {
        cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap_or(40),
        ..Default::default()
    };
    // SAFETY: `info.cbSize` is set as the API requires, and `info` is a live
    // out-parameter of exactly that size.
    if !unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool() {
        return Err(Error::Platform(
            "reading the monitor work area: GetMonitorInfoW refused, which \
             usually means the display was disconnected"
                .to_owned(),
        ));
    }
    let work = DeviceRect::new(
        info.rcWork.left,
        info.rcWork.top,
        info.rcWork.right,
        info.rcWork.bottom,
    );
    Ok(work_area_logical(work, dpi_for_monitor(monitor)))
}

/// The work area of the monitor nearest the pointer, in logical points.
///
/// The overlay follows the mouse, not the primary display: a capture taken on
/// the second monitor should show its card on the second monitor.
///
/// # Errors
///
/// [`Error::Platform`] if Windows would not report the cursor or the monitor.
pub fn work_area_under_pointer() -> Result<LogicalRect> {
    ensure_process_dpi_aware();
    let mut point = POINT::default();
    // SAFETY: live out-parameter.
    unsafe { GetCursorPos(&raw mut point) }.map_err(|e| map_err(e, "locating the pointer"))?;
    // SAFETY: `MonitorFromPoint` with DEFAULTTONEAREST always yields a monitor.
    let monitor =
        unsafe { windows::Win32::Graphics::Gdi::MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    monitor_work_area(monitor)
}

/// Where the pointer is inside `hwnd`, in window-local logical points.
///
/// `None` when the window is gone, when Windows will not say where the cursor
/// is, or when the pointer is genuinely outside the window. The caller —
/// egui's passthrough test — reads `None` as "nothing of ours is under the
/// mouse", so this must not return it out of mere uncertainty.
///
/// Takes the `HWND` as an `isize` because the probe is stored in an
/// `Arc<dyn Fn() + Send + Sync>` and `HWND` is a raw pointer, hence neither.
/// That is sound here: every call is a read-only query, and `IsWindow`,
/// `GetWindowRect` and `GetCursorPos` are all safe to make from any thread.
#[must_use]
pub fn pointer_in_hwnd(hwnd: isize) -> Option<(f64, f64)> {
    if hwnd == 0 {
        return None;
    }
    let handle = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: `IsWindow` exists to be asked about handles that may be stale.
    if !unsafe { IsWindow(Some(handle)) }.as_bool() {
        return None;
    }
    let mut rect = RECT::default();
    // SAFETY: live out-parameter; the handle was just validated.
    unsafe { GetWindowRect(handle, &raw mut rect) }.ok()?;
    let mut point = POINT::default();
    // SAFETY: live out-parameter.
    unsafe { GetCursorPos(&raw mut point) }.ok()?;
    // SAFETY: DEFAULTTONEAREST always yields a monitor for a real window.
    let monitor = unsafe { MonitorFromWindow(handle, MONITOR_DEFAULTTONEAREST) };
    let scale = scale_from_dpi(dpi_for_monitor(monitor));
    pointer_in_window(
        (point.x, point.y),
        DeviceRect::new(rect.left, rect.top, rect.right, rect.bottom),
        scale,
    )
}
