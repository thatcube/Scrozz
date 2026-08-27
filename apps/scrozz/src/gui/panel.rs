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
//! naming `eframe::CreationContext` requires `eframe` to be a direct dependency
//! of this crate, which it is not (see [`crate::gui::host::WINDOW_GAP`]). So the
//! work is split at the one place the split is free: the platform handle is a
//! raw pointer either way.
//!
//! [`convert_ns_view`] is the whole conversion and needs nothing but
//! `scrozz-shell`. The hook is then six lines of pointer extraction, written out
//! verbatim in [`HOOK_SOURCE`], which compile the moment `eframe` and
//! `raw-window-handle` are available. That ordering is deliberate: the part with
//! the platform risk in it is the part that can be compiled and tested today.
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

/// The hook body, ready to paste once `eframe` is a dependency.
///
/// Kept as source rather than prose because the thing most likely to go wrong
/// when someone writes it from memory is silently taking the `ns_window` arm on
/// a handle that reports `ns_view`, which produces a `PanelReport` that says
/// "converted" about the wrong window.
///
/// Assign it where [`crate::gui::host::for_platform`] builds `OverlayOptions`:
///
/// ```text
/// options.panel = Some(crate::gui::panel::hook());
/// ```
pub const HOOK_SOURCE: &str = r#"
/// Requires `eframe` and `raw-window-handle` in apps/scrozz/Cargo.toml.
pub fn hook() -> scrozz_ui::PanelHook {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    Box::new(|cc: &eframe::CreationContext<'_>| {
        let Ok(handle) = cc.window_handle() else {
            return scrozz_ui::PanelReport::unsupported("eframe reported no window handle");
        };
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return scrozz_ui::PanelReport::unsupported("the window is not an AppKit window");
        };
        // SAFETY: `handle` is borrowed for this scope, so the view is alive.
        unsafe { crate::gui::panel::convert_ns_view(appkit.ns_view.as_ptr()) }
    })
}
"#;

/// Why the hook itself is not compiled in yet.
pub const HOOK_GAP: &str = "the panel conversion is implemented and tested \
     (gui::panel::convert_ns_view); what is missing is the six-line PanelHook \
     that pulls the NSView pointer out of eframe's CreationContext. It needs \
     `eframe` (to name CreationContext) and `raw-window-handle` (for the \
     HasWindowHandle trait and the RawWindowHandle::AppKit arm) as direct \
     dependencies of apps/scrozz. Neither is declared, and raw-window-handle \
     is not in [workspace.dependencies] either. See gui::panel::HOOK_SOURCE";

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
    fn the_hook_source_names_the_view_arm_not_the_window_arm() {
        // eframe reports `ns_view`. Reaching for `from_ns_window` with a view
        // pointer type-checks (both are *mut c_void) and is wrong, so the
        // pasteable source must not model that mistake.
        assert!(HOOK_SOURCE.contains("ns_view"), "{HOOK_SOURCE}");
        assert!(!HOOK_SOURCE.contains("from_ns_window"), "{HOOK_SOURCE}");
        assert!(HOOK_SOURCE.contains("convert_ns_view"), "{HOOK_SOURCE}");
    }

    #[test]
    fn the_gap_names_both_missing_crates() {
        // A gap that names one of two required dependencies gets fixed once,
        // fails to compile, and gets called a broken suggestion.
        assert!(HOOK_GAP.contains("eframe"), "{HOOK_GAP}");
        assert!(HOOK_GAP.contains("raw-window-handle"), "{HOOK_GAP}");
    }
}
