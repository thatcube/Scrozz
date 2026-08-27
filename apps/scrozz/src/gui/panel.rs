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

#[cfg(target_os = "macos")]
use std::{cell::RefCell, rc::Rc};
use std::{
    ffi::c_void,
    sync::{Arc, Mutex},
};

use raw_window_handle::HasWindowHandle;
#[cfg(target_os = "macos")]
use raw_window_handle::RawWindowHandle;
use scrozz_core::Error;
use scrozz_shell::{NativeOverlay, NativeSurface, OverlayBehavior};
use scrozz_ui::PanelReport;

/// The live native view shared by panel setup and drag-out.
///
/// The pointer is stored as an address so this synchronization container stays
/// `Send + Sync`; reconstruction remains inside this app's audited unsafe
/// boundary and only happens on the main thread.
#[derive(Clone, Debug, Default)]
pub struct NativeSurfaceSlot {
    address: Arc<Mutex<Option<usize>>>,
}

impl NativeSurfaceSlot {
    fn remember(&self, view: *mut c_void) {
        if !view.is_null()
            && let Ok(mut address) = self.address.lock()
        {
            *address = Some(view as usize);
        }
    }

    /// Returns the live view captured during window creation.
    #[must_use]
    pub fn get(&self) -> Option<NativeSurface> {
        let address = self.address.lock().ok()?.as_ref().copied()?;
        // SAFETY: `remember` accepts only the NSView from eframe's live window
        // handle. The slot is owned by that same Windowed host and no drag can
        // outlive its event loop.
        Some(unsafe { NativeSurface::from_raw(address as *mut c_void) })
    }

    /// Changes panel visibility without taking keyboard focus.
    #[cfg(target_os = "macos")]
    pub fn set_visible_without_activation(&self, visible: bool) -> scrozz_core::Result<()> {
        let address = self
            .address
            .lock()
            .ok()
            .and_then(|address| *address)
            .ok_or_else(|| Error::TargetGone("the overlay NSView is unavailable".to_owned()))?;
        // SAFETY: the slot contains only the live NSView captured from eframe,
        // and this hook runs from the overlay's main-thread update.
        let overlay = unsafe { NativeOverlay::from_ns_view(address as *mut c_void)? };
        overlay.set_visible_without_activation(visible);
        Ok(())
    }
}

/// Main-thread control of the native window behavior after the creation hook.
///
/// The handle is intentionally `Rc`, not `Arc`: native window mutation belongs
/// to the one eframe thread and must never become callable from a capture worker.
#[derive(Clone, Default)]
pub struct BehaviorController {
    surface: NativeSurfaceSlot,
    #[cfg(target_os = "macos")]
    overlay: Rc<RefCell<Option<NativeOverlay>>>,
}

impl BehaviorController {
    /// Uses `surface` for native drag and visibility consumers.
    #[must_use]
    pub fn with_surface(surface: NativeSurfaceSlot) -> Self {
        Self {
            surface,
            #[cfg(target_os = "macos")]
            overlay: Rc::default(),
        }
    }

    /// Applies a card or selection behavior when a native adapter is retained.
    pub fn apply(&self, behavior: &OverlayBehavior) {
        #[cfg(target_os = "macos")]
        if let Some(overlay) = self.overlay.borrow_mut().as_mut()
            && let Err(error) = overlay.apply(behavior)
        {
            tracing::warn!(%error, "could not update native overlay behavior");
        }

        #[cfg(not(target_os = "macos"))]
        let _ = behavior;
    }

    #[cfg(target_os = "macos")]
    fn install(&self, overlay: NativeOverlay) {
        *self.overlay.borrow_mut() = Some(overlay);
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

/// The panel hook while retaining the view for native drag-out.
#[must_use]
pub fn hook_with_surface(surface: NativeSurfaceSlot) -> scrozz_ui::PanelHook {
    hook_with_controller(BehaviorController::with_surface(surface))
}

/// The creation hook plus a handle for later card/selector behavior changes.
#[must_use]
pub fn hook_with_controller(controller: BehaviorController) -> scrozz_ui::PanelHook {
    hook_configured(controller, true)
}

/// Retains the native view without converting its window into a panel.
#[must_use]
pub fn hook_without_conversion(controller: BehaviorController) -> scrozz_ui::PanelHook {
    hook_configured(controller, false)
}

fn hook_configured(controller: BehaviorController, convert: bool) -> scrozz_ui::PanelHook {
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
            let view = appkit.ns_view.as_ptr();
            controller.surface.remember(view);
            if !convert {
                PanelReport::unsupported("the panel conversion is disabled")
            } else {
                let (report, overlay) =
                    // SAFETY: the handle borrow keeps the NSView alive for adoption.
                    unsafe { convert_and_retain_ns_view(view) };
                if let Some(overlay) = overlay {
                    controller.install(overlay);
                }
                report
            }
        }

        #[cfg(not(target_os = "macos"))]
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
