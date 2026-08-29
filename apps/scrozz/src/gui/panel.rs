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
//! [`PanelHook`] is `FnOnce(&eframe::CreationContext<'_>) -> PanelSetup`, and
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
//! Refusal is not breakage: the overlay still draws and still works, it just
//! takes focus when clicked. That is the whole reason [`PanelReport`] carries a
//! `detail` string rather than being a bool.

use std::ffi::c_void;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(target_os = "macos")]
use scrozz_shell::OverlayWindow;
use scrozz_shell::{NativeOverlay, OverlayBehavior};
use scrozz_ui::{PanelReport, PanelSetup};

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
    #[cfg(target_os = "macos")]
    {
        // SAFETY: forwarded from this function's contract.
        unsafe { setup_ns_view(ns_view, true) }.report
    }

    #[cfg(not(target_os = "macos"))]
    {
        if ns_view.is_null() {
            return PanelReport::unsupported("the window handle carried a null NSView");
        }

        // The entry point is named differently per platform: macOS adopts a view or
        // a window, and the stub backends adopt an opaque handle. Both refuse
        // safely, so the non-macOS arm is a real path rather than a `todo!`.
        // SAFETY: as above; the stub backends do not dereference the handle.
        let adopted = unsafe { NativeOverlay::adopt(ns_view) };

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
}

#[cfg(target_os = "macos")]
unsafe fn setup_ns_view(ns_view: *mut c_void, convert_panel: bool) -> PanelSetup {
    if ns_view.is_null() {
        return PanelSetup::unsupported("the window handle carried a null NSView");
    }

    // SAFETY: forwarded from this function's own contract — a live `NSView *`
    // on the main thread.
    let mut overlay = match unsafe { NativeOverlay::from_ns_view(ns_view) } {
        Ok(overlay) => overlay,
        Err(err) => return PanelSetup::unsupported(err.to_string()),
    };
    let report = if convert_panel {
        match overlay.apply(&OverlayBehavior::capture_card()) {
            Ok(report) if report.non_activating => PanelReport::converted(report.detail),
            Ok(report) => PanelReport::unsupported(report.detail),
            Err(err) => PanelReport::unsupported(err.to_string()),
        }
    } else {
        PanelReport::unsupported("native panel conversion was disabled by SCROZZ_GUI_PANEL")
    };

    PanelSetup::new(report).with_passthrough(Box::new(move |requested| {
        overlay
            .set_click_through(requested)
            .map_err(|err| err.to_string())?;
        overlay
            .click_through()
            .map(|actual| actual == requested)
            .map_err(|err| err.to_string())
    }))
}

#[cfg(target_os = "windows")]
fn setup_windows(hwnd: isize) -> PanelSetup {
    use windows::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            GWL_EXSTYLE, GetWindowLongPtrW, SetWindowLongPtrW, WS_EX_LAYERED, WS_EX_TRANSPARENT,
        },
    };

    let hwnd = HWND(hwnd as *mut c_void);
    PanelSetup::unsupported(
        "the Windows overlay is not yet converted into a non-activating native panel",
    )
    .with_passthrough(Box::new(move |requested| {
        let mask = (WS_EX_LAYERED | WS_EX_TRANSPARENT).0 as isize;
        // SAFETY: `hwnd` came from eframe's live `WindowHandle`; this closure is
        // retained only by that window's app and runs on its UI thread.
        let current = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
        let requested_style = if requested {
            current | mask
        } else {
            current & !mask
        };
        if requested_style != current {
            // SAFETY: as above. Readback below is the success criterion because
            // zero is both a valid previous style and the API's failure value.
            unsafe {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, requested_style);
            }
        }
        // SAFETY: as above.
        let actual = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
        Ok((actual & mask == mask) == requested)
    }))
}

#[cfg(target_os = "linux")]
fn setup_x11(window: u32) -> PanelSetup {
    use x11rb::{
        connection::Connection,
        protocol::{
            shape::{ConnectionExt as _, SK, SO},
            xproto::{ClipOrdering, ConnectionExt as _, Rectangle},
        },
    };

    let (connection, _) = match x11rb::connect(None) {
        Ok(connection) => connection,
        Err(err) => {
            return PanelSetup::unsupported(format!(
                "the X11 overlay connection could not be opened: {err}"
            ));
        }
    };

    PanelSetup::unsupported(
        "the X11 overlay is not yet converted into a non-activating native panel",
    )
    .with_passthrough(Box::new(move |requested| {
        let rectangles = if requested {
            Vec::new()
        } else {
            let geometry = connection
                .get_geometry(window)
                .map_err(|err| err.to_string())?
                .reply()
                .map_err(|err| err.to_string())?;
            vec![Rectangle {
                x: 0,
                y: 0,
                width: geometry.width,
                height: geometry.height,
            }]
        };
        connection
            .shape_rectangles(
                SO::SET,
                SK::INPUT,
                ClipOrdering::UNSORTED,
                window,
                0,
                0,
                &rectangles,
            )
            .map_err(|err| err.to_string())?
            .check()
            .map_err(|err| err.to_string())?;
        connection.flush().map_err(|err| err.to_string())?;
        let actual = connection
            .shape_get_rectangles(window, SK::INPUT)
            .map_err(|err| err.to_string())?
            .reply()
            .map_err(|err| err.to_string())?;
        Ok(actual.rectangles.is_empty() == requested)
    }))
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
    hook_with_conversion(true)
}

/// Builds the native-window hook while optionally leaving the macOS window's
/// activation class unchanged. Click-through control remains installed either
/// way because automatic scrolling must not depend on the panel diagnostic.
#[must_use]
pub fn hook_with_conversion(convert_panel: bool) -> scrozz_ui::PanelHook {
    Box::new(move |cc: &eframe::CreationContext<'_>| {
        let handle = match cc.window_handle() {
            Ok(handle) => handle,
            Err(err) => {
                return PanelSetup::unsupported(format!("eframe reported no window handle: {err}"));
            }
        };

        #[cfg(target_os = "macos")]
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return PanelSetup::unsupported(
                "the overlay window is not an AppKit window, so it has no NSView to convert",
            );
        };

        // SAFETY: `handle` borrows the window for this scope, so the view is
        // alive; `OverlayApp::new` runs on the main thread.
        #[cfg(target_os = "macos")]
        return unsafe { setup_ns_view(appkit.ns_view.as_ptr(), convert_panel) };

        #[cfg(target_os = "windows")]
        {
            let RawWindowHandle::Win32(win32) = handle.as_raw() else {
                return PanelSetup::unsupported(
                    "the overlay window is not a Win32 window, so native passthrough is unavailable",
                );
            };
            return setup_windows(win32.hwnd.get());
        }

        #[cfg(target_os = "linux")]
        {
            match handle.as_raw() {
                RawWindowHandle::Xlib(xlib) => u32::try_from(xlib.window).map_or_else(
                    |_| PanelSetup::unsupported("the X11 window ID does not fit in 32 bits"),
                    setup_x11,
                ),
                RawWindowHandle::Xcb(xcb) => setup_x11(xcb.window.get()),
                RawWindowHandle::Wayland(_) => PanelSetup::unsupported(
                    "Wayland automatic scrolling is manual-only, so native passthrough is not required",
                ),
                _ => PanelSetup::unsupported(
                    "the Linux overlay has neither an X11 nor a Wayland window handle",
                ),
            }
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
