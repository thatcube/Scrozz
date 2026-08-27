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
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;

use raw_window_handle::HasWindowHandle;
#[cfg(target_os = "macos")]
use raw_window_handle::RawWindowHandle;
#[cfg(target_os = "macos")]
use scrozz_shell::OverlayWindow;
use scrozz_shell::{NativeOverlay, OverlayBehavior};
use scrozz_ui::{PanelReport, overlay_app::NativePinRequest};

/// Native child-window adapters retained for the lifetime of pinned viewports.
#[derive(Default)]
pub struct PinPanels {
    #[cfg(target_os = "macos")]
    windows: HashMap<String, NativeOverlay>,
    #[cfg(target_os = "macos")]
    pending_logged: HashSet<String>,
}

impl PinPanels {
    /// Adopt newly-created pin viewports and apply their current native state.
    pub fn reconcile(&mut self, requests: &[NativePinRequest]) {
        #[cfg(target_os = "macos")]
        self.reconcile_macos(requests);

        #[cfg(not(target_os = "macos"))]
        let _ = requests;
    }

    #[cfg(target_os = "macos")]
    fn reconcile_macos(&mut self, requests: &[NativePinRequest]) {
        let live: HashSet<&str> = requests
            .iter()
            .map(|request| request.title.as_str())
            .collect();
        let stale: Vec<String> = self
            .windows
            .keys()
            .filter(|title| !live.contains(title.as_str()))
            .cloned()
            .collect();
        for title in stale {
            if let Some(mut overlay) = self.windows.remove(&title)
                && let Err(err) = overlay.restore_native_class()
            {
                tracing::warn!(window = %title, "could not restore pin window before close: {err}");
            }
            self.pending_logged.remove(&title);
        }

        for request in requests {
            if !self.windows.contains_key(&request.title) {
                match NativeOverlay::find_by_title(&request.title) {
                    Ok(Some(window)) => {
                        self.windows.insert(request.title.clone(), window);
                        self.pending_logged.remove(&request.title);
                    }
                    Ok(None) => {
                        if self.pending_logged.insert(request.title.clone()) {
                            tracing::debug!(
                                window = %request.title,
                                "pin child viewport is not native-visible yet; adoption will retry"
                            );
                        }
                        continue;
                    }
                    Err(err) => {
                        tracing::warn!(
                            window = %request.title,
                            "could not discover pin child viewport: {err}"
                        );
                        continue;
                    }
                }
            }

            let Some(window) = self.windows.get_mut(&request.title) else {
                continue;
            };
            let mut behavior = OverlayBehavior::pinned_capture(request.state.locked);
            behavior.opacity = request.state.opacity;
            behavior.has_shadow = request.shadow;
            if let Err(err) = window.apply(&behavior) {
                tracing::warn!(window = %request.title, "could not apply native pin behavior: {err}");
            }
            if let Err(err) = window.set_frame(request.state.frame) {
                tracing::warn!(window = %request.title, "could not apply native pin geometry: {err}");
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for PinPanels {
    fn drop(&mut self) {
        for (title, window) in &mut self.windows {
            if let Err(err) = window.restore_native_class() {
                tracing::warn!(window = %title, "could not restore pin window during teardown: {err}");
            }
        }
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
    if ns_view.is_null() {
        return PanelReport::unsupported("the window handle carried a null NSView");
    }

    // The entry point is named differently per platform: macOS adopts a view or
    // a window, and the stub backends adopt an opaque handle. Both refuse
    // safely, so the non-macOS arm is a real path rather than a `todo!`.
    #[cfg(target_os = "macos")]
    // SAFETY: forwarded from this function's own contract — a live `NSView *`
    // on the main thread.
    let adopted = unsafe { NativeOverlay::from_ns_view(ns_view) };

    #[cfg(not(target_os = "macos"))]
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

        #[cfg(not(target_os = "macos"))]
        {
            let _ = handle;
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
