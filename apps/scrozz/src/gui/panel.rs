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
//! Refusal is not breakage: the overlay still draws and still works, it just
//! takes focus when clicked. That is the whole reason [`PanelReport`] carries a
//! `detail` string rather than being a bool.

use std::ffi::c_void;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::{cell::RefCell, rc::Rc};

use raw_window_handle::HasWindowHandle;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use raw_window_handle::RawWindowHandle;
use scrozz_core::LogicalRect;
#[cfg(target_os = "macos")]
use scrozz_shell::OverlayWindow;
use scrozz_shell::{NativeOverlay, OverlayBehavior};
use scrozz_ui::PanelReport;

/// Main-thread control of the native window behavior after the creation hook.
///
/// The handle is intentionally `Rc`, not `Arc`: native window mutation belongs
/// to the one eframe thread and must never become callable from a capture worker.
#[derive(Clone, Default)]
pub struct BehaviorController {
    #[cfg(target_os = "macos")]
    overlay: Rc<RefCell<Option<NativeOverlay>>>,
    #[cfg(target_os = "linux")]
    x11_focus: Rc<RefCell<Option<scrozz_shell::X11FocusLease>>>,
    #[cfg(test)]
    behavior_log: Rc<RefCell<Vec<OverlayBehavior>>>,
}

impl BehaviorController {
    /// Re-anchors the retained native window to an exact OS work-area frame.
    ///
    /// Eframe's viewport position is only a window-manager hint. The native
    /// frame is authoritative on macOS and keeps the transparent overlay clipped
    /// above the Dock after creation, selection and display changes.
    pub fn set_frame(&self, frame: LogicalRect) {
        #[cfg(target_os = "macos")]
        if let Some(overlay) = self.overlay.borrow_mut().as_mut()
            && let Err(error) = overlay.set_frame(frame)
        {
            tracing::warn!(%error, "could not re-anchor native overlay frame");
        }

        #[cfg(not(target_os = "macos"))]
        let _ = frame;
    }

    /// Applies a card or selection behavior when a native adapter is retained.
    pub fn apply(&self, behavior: &OverlayBehavior) {
        #[cfg(target_os = "macos")]
        if let Some(overlay) = self.overlay.borrow_mut().as_mut()
            && let Err(error) = overlay.apply(behavior)
        {
            tracing::warn!(%error, "could not update native overlay behavior");
        }

        #[cfg(target_os = "linux")]
        if let Some(focus) = self.x11_focus.borrow_mut().as_mut()
            && let Err(error) = focus.set_wants_focus(behavior.accepts_key)
        {
            tracing::warn!(%error, "could not update X11 selector keyboard focus");
        }

        #[cfg(test)]
        self.behavior_log.borrow_mut().push(*behavior);

        #[cfg(not(any(target_os = "macos", target_os = "linux", test)))]
        let _ = behavior;
    }

    /// Retries native behavior that had to wait for queued window commands.
    pub fn refresh(&self) {
        #[cfg(target_os = "linux")]
        if let Some(focus) = self.x11_focus.borrow_mut().as_mut()
            && let Err(error) = focus.refresh()
        {
            tracing::warn!(%error, "could not acquire X11 selector keyboard focus");
        }
    }

    #[cfg(target_os = "macos")]
    fn install(&self, overlay: NativeOverlay) {
        *self.overlay.borrow_mut() = Some(overlay);
    }

    #[cfg(target_os = "linux")]
    fn install_x11_focus(&self, focus: scrozz_shell::X11FocusLease) {
        *self.x11_focus.borrow_mut() = Some(focus);
    }

    #[cfg(test)]
    pub(crate) fn recording() -> (Self, Rc<RefCell<Vec<OverlayBehavior>>>) {
        let controller = Self::default();
        (controller.clone(), Rc::clone(&controller.behavior_log))
    }
}

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
    // SAFETY: forwarded from this function's own contract.
    unsafe { convert_and_retain_ns_view(ns_view) }.0
}

#[cfg(target_os = "macos")]
unsafe fn convert_and_retain_ns_view(ns_view: *mut c_void) -> (PanelReport, Option<NativeOverlay>) {
    if ns_view.is_null() {
        return (
            PanelReport::unsupported("the window handle carried a null NSView"),
            None,
        );
    }

    // SAFETY: forwarded from this function's own contract — a live `NSView *`
    // on the main thread.
    let adopted = unsafe { NativeOverlay::from_ns_view(ns_view) };

    let mut overlay = match adopted {
        Ok(overlay) => overlay,
        Err(err) => return (PanelReport::unsupported(err.to_string()), None),
    };

    let report = match overlay.apply(&OverlayBehavior::capture_card()) {
        // `non_activating` is the only part of the behaviour D27 depends on.
        // Everything else — level, collection behaviour — can be applied and
        // the card still behaves; this cannot.
        Ok(report) if report.non_activating => PanelReport::converted(report.detail),
        Ok(report) => PanelReport::unsupported(report.detail),
        Err(err) => PanelReport::unsupported(err.to_string()),
    };
    (report, Some(overlay))
}

#[cfg(not(target_os = "macos"))]
unsafe fn convert_and_retain_ns_view(ns_view: *mut c_void) -> (PanelReport, Option<NativeOverlay>) {
    if ns_view.is_null() {
        return (
            PanelReport::unsupported("the window handle carried a null native view"),
            None,
        );
    }
    (
        PanelReport::unsupported("only the macOS native overlay adapter is retained by this hook"),
        None,
    )
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
    hook_with_controller(BehaviorController::default())
}

/// The creation hook plus a handle for later card/selector behavior changes.
#[must_use]
pub fn hook_with_controller(controller: BehaviorController) -> scrozz_ui::PanelHook {
    Box::new(move |cc: &eframe::CreationContext<'_>| {
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
        {
            let (report, overlay) =
                // SAFETY: the handle borrow keeps the NSView alive for adoption.
                unsafe { convert_and_retain_ns_view(appkit.ns_view.as_ptr()) };
            if let Some(overlay) = overlay {
                controller.install(overlay);
            }
            report
        }

        #[cfg(target_os = "linux")]
        {
            let detail = match attach_x11_focus_handle(handle.as_raw(), &controller) {
                Ok(()) => {
                    "attached direct X11 selection focus; native capture-card input shaping is \
                     still unavailable on this branch"
                        .to_owned()
                }
                Err(error) => error,
            };
            PanelReport::unsupported(detail)
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = handle;
            let _ = controller;
            PanelReport::unsupported(
                "only the macOS overlay backend is implemented so far, so the \
                 window keeps its native activation behaviour",
            )
        }
    })
}

/// Attaches direct keyboard-focus ownership to an eframe X11 window.
///
/// This is separate from panel conversion so the one-shot selector can retain
/// X11 focus without attempting the macOS-only AppKit conversion.
#[cfg(target_os = "linux")]
pub fn attach_x11_focus(
    cc: &eframe::CreationContext<'_>,
    controller: &BehaviorController,
) -> Result<(), String> {
    let handle = cc
        .window_handle()
        .map_err(|error| format!("eframe reported no window handle for X11 focus: {error}"))?;
    attach_x11_focus_handle(handle.as_raw(), controller)
}

#[cfg(target_os = "linux")]
fn attach_x11_focus_handle(
    handle: RawWindowHandle,
    controller: &BehaviorController,
) -> Result<(), String> {
    let window = match handle {
        RawWindowHandle::Xlib(xlib) => u32::try_from(xlib.window).map_err(|_| {
            format!(
                "the X11 window ID {} does not fit the protocol's 32-bit window field",
                xlib.window
            )
        })?,
        RawWindowHandle::Xcb(xcb) => xcb.window.get(),
        RawWindowHandle::Wayland(_) => {
            return Err(
                "Wayland controls keyboard focus; direct X11 focus is unavailable".to_owned(),
            );
        }
        other => {
            return Err(format!(
                "the selector window is neither X11 nor Wayland ({other:?})"
            ));
        }
    };

    match scrozz_shell::X11FocusLease::adopt(window) {
        Ok(focus) => {
            controller.install_x11_focus(focus);
            Ok(())
        }
        Err(error) => Err(error.to_string()),
    }
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
