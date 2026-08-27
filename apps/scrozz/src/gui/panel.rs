//! Turning the overlay's window into a non-activating panel.
//!
//! # Why this lives here
//!
//! `scrozz-ui` is `#![forbid(unsafe_code)]` and deliberately does not depend on
//! `scrozz-shell`, so it cannot perform the conversion itself; it exposes a
//! [`PanelHook`] instead. This crate depends on both, so this is the only place
//! the two halves can meet.
//!
//! # Why it takes a pointer
//!
//! [`PanelHook`] is `FnOnce(&eframe::CreationContext<'_>) -> PanelReport`, and
//! the work is split in two: [`hook`] does the pointer extraction, and
//! [`convert_ns_view`] does the conversion. The split is where the platform
//! risk is — the conversion can be exercised without a window, so it is the
//! part that carries the tests.
//!
//! # What the conversion actually does
//!
//! AppKit has no "make this window non-activating" switch. The behaviour lives
//! on `NSPanel` + `NSWindowStyleMaskNonactivatingPanel`, and winit hands us an
//! `NSWindow`. `scrozz-shell` isa-swizzles the instance into a runtime-built
//! `NSPanel` subclass and then sets the mask, guarding the swizzle on the two
//! classes having identical instance sizes and refusing when they do not.
//!
//! Windows *does* have a switch — `WS_EX_NOACTIVATE` — but winit rewrites the
//! whole extended-style word whenever any of the flags it models changes, and
//! it does not model that bit. `scrozz-shell` therefore treats the style as a
//! specification and re-asserts it from a `WM_STYLECHANGING` hook rather than
//! writing it once. Both platforms end up in the same place by very different
//! routes, which is exactly why the seam is a hook and not a flag.
//!
//! Refusal is not breakage: the overlay still draws and still works, it just
//! takes focus when clicked. That is the whole reason [`PanelReport`] carries a
//! `detail` string rather than being a bool.

use std::ffi::c_void;

use raw_window_handle::HasWindowHandle;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use raw_window_handle::RawWindowHandle;
use scrozz_shell::{NativeOverlay, OverlayBehavior};
use scrozz_ui::PanelReport;

/// Converts the window hosting `ns_view` into a non-activating overlay panel.
///
/// `ns_view` is the `ns_view` field of a `RawWindowHandle::AppKit`, which is
/// what `eframe::CreationContext` reports on macOS. A null pointer is refused
/// rather than dereferenced.
///
/// Never panics and never fails: every outcome, including "this platform has no
/// overlay backend", comes back as a [`PanelReport`]. A hook that could fail
/// would be a hook that can take down the window it was called to configure.
///
/// # Safety
///
/// `ns_view` must be a live `NSView *` whose `WindowHandle` borrow is still
/// alive, and this must be called on the main thread. Both hold inside the
/// `eframe` app creator, which is the only caller.
#[must_use]
pub unsafe fn convert_ns_view(ns_view: *mut c_void) -> PanelReport {
    if ns_view.is_null() {
        return PanelReport::unsupported("the window handle carried a null NSView");
    }

    // The entry point is named differently per platform: macOS adopts a view or
    // a window, and the other backends adopt an opaque handle. All of them
    // refuse safely, so the non-macOS arm is a real path rather than a `todo!`.
    #[cfg(target_os = "macos")]
    // SAFETY: forwarded from this function's own contract — a live `NSView *`
    // on the main thread.
    let adopted = unsafe { NativeOverlay::from_ns_view(ns_view) };

    #[cfg(not(target_os = "macos"))]
    // SAFETY: as above. The stub backends do not dereference the handle; the
    // Windows backend validates it with `IsWindow` before doing anything else.
    let adopted = unsafe { NativeOverlay::adopt(ns_view) };

    finish(adopted)
}

/// Configures the overlay window identified by `hwnd` as a Windows overlay.
///
/// `hwnd` is the `hwnd` field of a `RawWindowHandle::Win32`, which is what
/// `eframe::CreationContext` reports on Windows. Unlike AppKit, the handle
/// *is* the window: there is no view to ask for its window.
///
/// Never panics and never fails, for the same reason [`convert_ns_view`] does
/// not: a hook that could fail is a hook that can take down the window it was
/// called to configure.
///
/// # Safety
///
/// `hwnd` must be a live `HWND` whose `WindowHandle` borrow is still alive, and
/// this must be called on the thread that owns it. Both hold inside the
/// `eframe` app creator, which is the only caller.
#[cfg(target_os = "windows")]
#[must_use]
pub unsafe fn convert_hwnd(hwnd: *mut c_void) -> PanelReport {
    if hwnd.is_null() {
        return PanelReport::unsupported("the window handle carried a null HWND");
    }
    // Published before the conversion is attempted, because the pointer probe
    // is useful whether or not the style rewrite succeeded: a card that pulls
    // focus should still know where the mouse is.
    OVERLAY_HWND.store(hwnd as isize, std::sync::atomic::Ordering::Relaxed);
    // SAFETY: forwarded from this function's own contract.
    finish(unsafe { NativeOverlay::adopt(hwnd) })
}

/// Configures a Windows overlay whose pixels come from `UpdateLayeredWindow`.
///
/// This differs from [`convert_hwnd`] only in layered-window initialization:
/// calling `SetLayeredWindowAttributes` first would make the later
/// `UpdateLayeredWindow` fail by documented Win32 contract.
///
/// # Safety
///
/// As [`convert_hwnd`].
#[cfg(target_os = "windows")]
#[must_use]
pub unsafe fn convert_hwnd_layered_bitmap(hwnd: *mut c_void) -> PanelReport {
    if hwnd.is_null() {
        return PanelReport::unsupported("the window handle carried a null HWND");
    }
    OVERLAY_HWND.store(hwnd as isize, std::sync::atomic::Ordering::Relaxed);

    // SAFETY: forwarded from this function's own contract.
    let mut overlay = match unsafe { NativeOverlay::adopt(hwnd) } {
        Ok(overlay) => overlay,
        Err(error) => return PanelReport::unsupported(error.to_string()),
    };
    match overlay.apply_layered_bitmap(&OverlayBehavior::capture_card()) {
        Ok(report) if report.non_activating => PanelReport::converted(report.detail),
        Ok(report) => PanelReport::unsupported(report.detail),
        Err(error) => PanelReport::unsupported(error.to_string()),
    }
}

/// The overlay window's `HWND`, as an integer, or `0` before it exists.
///
/// An integer rather than an `HWND` because the one consumer — the pointer
/// probe in [`crate::gui::host`] — lives behind `Arc<dyn Fn() + Send + Sync>`,
/// and `HWND` is a raw pointer, so it is neither `Send` nor `Sync`. Storing the
/// bits keeps that boundary honest instead of forcing an `unsafe impl`.
///
/// Reading it can race with the window closing. That is not a soundness
/// problem: every consumer passes the value straight to `IsWindow`, which
/// exists precisely to be asked about handles that may already be dead.
#[cfg(target_os = "windows")]
static OVERLAY_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// The overlay window's `HWND` as an integer, or `0` if there is not one yet.
#[cfg(target_os = "windows")]
#[must_use]
pub fn overlay_hwnd() -> isize {
    OVERLAY_HWND.load(std::sync::atomic::Ordering::Relaxed)
}

/// Applies the capture-card behaviour to a freshly adopted overlay.
///
/// Shared by both entry points because the interesting half — what counts as
/// success — must not be allowed to drift between platforms.
fn finish(adopted: scrozz_core::Result<NativeOverlay>) -> PanelReport {
    let mut overlay = match adopted {
        Ok(overlay) => overlay,
        Err(err) => return PanelReport::unsupported(err.to_string()),
    };

    match overlay.apply(&OverlayBehavior::capture_card()) {
        // `non_activating` is the only part of the behaviour D27 depends on.
        // Everything else — level, collection behaviour — can be applied and
        // the card still behaves; this cannot.
        Ok(report) if report.non_activating => PanelReport::converted(report.detail),
        Ok(report) => PanelReport::unsupported(report.detail),
        Err(err) => PanelReport::unsupported(err.to_string()),
    }
}

/// The hook `scrozz-ui` runs while the overlay window is being created.
///
/// Extracts the platform handle from the `CreationContext` and hands it to
/// [`convert_ns_view`]. That is the whole hook: everything with platform risk
/// in it lives in the conversion, which is unit-tested without a window.
///
/// The `ns_view` / `ns_window` distinction is the subtle one. `eframe` reports
/// an **`NSView`**, and `scrozz-shell` offers `from_ns_view` and
/// `from_ns_window` — both taking `*mut c_void`, so handing a view to the
/// window entry point type-checks and then converts the wrong object. A test
/// pins the arm this reaches for.
#[must_use]
pub fn hook() -> scrozz_ui::PanelHook {
    Box::new(|cc: &eframe::CreationContext<'_>| {
        let handle = match cc.window_handle() {
            Ok(handle) => handle,
            Err(err) => {
                return PanelReport::unsupported(format!(
                    "eframe reported no window handle: {err}"
                ));
            }
        };

        #[cfg(target_os = "macos")]
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return PanelReport::unsupported(
                "the overlay window is not an AppKit window, so it has no NSView to convert",
            );
        };

        // SAFETY: `handle` borrows the window for this scope, so the view is
        // alive; `OverlayApp::new` runs on the main thread.
        #[cfg(target_os = "macos")]
        return unsafe { convert_ns_view(appkit.ns_view.as_ptr()) };

        #[cfg(target_os = "windows")]
        let RawWindowHandle::Win32(win32) = handle.as_raw() else {
            return PanelReport::unsupported(
                "the overlay window is not a Win32 window, so it has no HWND to configure",
            );
        };

        // SAFETY: `handle` borrows the window for this scope, so the HWND is
        // alive, and `raw-window-handle` documents it as belonging to the
        // calling thread — which is the event-loop thread, as the adapter
        // requires.
        #[cfg(target_os = "windows")]
        return unsafe { convert_hwnd(win32.hwnd.get() as *mut c_void) };

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = handle;
            PanelReport::unsupported(
                "only the macOS and Windows overlay backends are implemented so \
                 far, so the window keeps its native activation behaviour",
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_null_view_is_refused_rather_than_dereferenced() {
        // The one input that would be undefined behaviour if it got through.
        // `scrozz-shell` checks this too; checking it here as well means the
        // guarantee does not depend on which backend the target selects.
        let report = unsafe { convert_ns_view(std::ptr::null_mut()) };
        assert!(!report.non_activating);
        assert!(report.detail.contains("null"), "{}", report.detail);
    }

    #[test]
    fn refusal_is_reported_not_raised() {
        // The property the whole hook design rests on: a hook that could fail
        // is a hook that can take down the window it was called to configure.
        // Every path returns a report, so the overlay always survives.
        let report = unsafe { convert_ns_view(std::ptr::null_mut()) };
        assert!(
            !report.detail.is_empty(),
            "a refusal with no reason is indistinguishable from a bug"
        );
    }

    #[test]
    fn the_hook_reaches_for_the_view_arm_not_the_window_arm() {
        // eframe reports `ns_view`. Reaching for `from_ns_window` with a view
        // pointer type-checks (both are *mut c_void) and converts the wrong
        // object, so this pins the source rather than trusting review.
        let source = include_str!("panel.rs");
        let body = source
            .split("pub fn hook()")
            .nth(1)
            .expect("the hook is defined in this file")
            .split("#[cfg(test)]")
            .next()
            .expect("the hook ends before the tests do");
        assert!(body.contains("ns_view"), "the hook must use the NSView arm");
        assert!(
            !body.contains("from_ns_window"),
            "the hook must not convert the window handle as if it were a view"
        );
    }

    #[test]
    fn a_hook_can_be_built_without_a_window() {
        // Building the hook must not touch AppKit — it is constructed at
        // start-up, long before `eframe` has a window to hand it.
        let hook = hook();
        drop(hook);
    }
}
