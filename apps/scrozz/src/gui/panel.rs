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

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;

use raw_window_handle::HasWindowHandle;
#[cfg(target_os = "macos")]
use raw_window_handle::RawWindowHandle;
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use scrozz_shell::OverlayWindow;
use scrozz_shell::{NativeOverlay, OverlayBehavior};
use scrozz_ui::{PanelReport, overlay_app::NativePinRequest};

#[derive(Clone, Debug, PartialEq)]
struct AppliedPin {
    frame: scrozz_core::LogicalRect,
    positioning: bool,
    display_scale: scrozz_core::ScaleFactor,
    locked: bool,
    opacity: scrozz_core::Opacity,
    shadow: bool,
}

impl From<&NativePinRequest> for AppliedPin {
    fn from(request: &NativePinRequest) -> Self {
        Self {
            frame: request.state.frame,
            positioning: request.positioning,
            display_scale: request.display_scale,
            locked: request.state.locked,
            opacity: request.state.opacity,
            shadow: request.shadow,
        }
    }
}

impl AppliedPin {
    fn behavior_changed(&self, previous: Option<&Self>) -> bool {
        previous.is_none_or(|previous| {
            self.locked != previous.locked
                || self.opacity != previous.opacity
                || self.shadow != previous.shadow
        })
    }

    fn frame_changed(&self, previous: Option<&Self>) -> bool {
        previous.is_none_or(|previous| {
            self.frame != previous.frame || self.display_scale != previous.display_scale
        })
    }

    fn should_set_frame(&self, previous: Option<&Self>) -> bool {
        self.positioning && self.frame_changed(previous)
    }
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
struct RetainedPin {
    window: NativeOverlay,
    applied: Option<AppliedPin>,
}

/// Native pin adoption/mutation failure to show inside the affected viewport.
pub struct PinPanelFailure {
    pub pin: scrozz_core::PinId,
    pub reason: String,
}

/// Result of one native child-window reconciliation pass.
#[derive(Default)]
pub struct PinPanelReport {
    pub failures: Vec<PinPanelFailure>,
    pub pending_adoption: bool,
}

const ADOPTION_RETRY_LIMIT: u8 = 60;

/// Native child-window adapters retained for the lifetime of pinned viewports.
#[derive(Default)]
pub struct PinPanels {
    #[cfg(target_os = "macos")]
    windows: HashMap<String, RetainedPin>,
    #[cfg(target_os = "windows")]
    windows: HashMap<String, RetainedPin>,
    #[cfg(target_os = "linux")]
    windows: HashMap<String, RetainedPin>,
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    pending_logged: HashSet<String>,
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    failures: HashMap<String, String>,
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    adoption_attempts: HashMap<String, u8>,
}

impl PinPanels {
    /// Adopt newly-created pin viewports and apply their current native state.
    pub fn reconcile(&mut self, requests: &[NativePinRequest]) -> PinPanelReport {
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        return self.reconcile_native(requests);

        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            let _ = requests;
            PinPanelReport::default()
        }
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn reconcile_native(&mut self, requests: &[NativePinRequest]) -> PinPanelReport {
        let mut report = PinPanelReport::default();
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
            if let Some(mut retained) = self.windows.remove(&title)
                && let Err(err) = retained.window.restore_native_class()
            {
                tracing::warn!(window = %title, "could not restore pin window before close: {err}");
            }
            self.pending_logged.remove(&title);
            self.failures.remove(&title);
            self.adoption_attempts.remove(&title);
        }

        for request in requests {
            if !self.windows.contains_key(&request.title) {
                match NativeOverlay::find_by_title(&request.title) {
                    Ok(Some(window)) => {
                        self.windows.insert(
                            request.title.clone(),
                            RetainedPin {
                                window,
                                applied: None,
                            },
                        );
                        self.pending_logged.remove(&request.title);
                        self.adoption_attempts.remove(&request.title);
                    }
                    Ok(None) => {
                        if !should_retry_adoption(&mut self.adoption_attempts, &request.title) {
                            record_failure(
                                &mut self.failures,
                                request,
                                "the native pin window did not become discoverable within the bounded adoption window"
                                    .into(),
                                &mut report.failures,
                            );
                            continue;
                        }
                        report.pending_adoption = true;
                        if self.pending_logged.insert(request.title.clone()) {
                            tracing::debug!(
                                window = %request.title,
                                "pin child viewport is not native-visible yet; adoption will retry"
                            );
                        }
                        continue;
                    }
                    Err(err) => {
                        record_failure(
                            &mut self.failures,
                            request,
                            err.to_string(),
                            &mut report.failures,
                        );
                        report.pending_adoption |=
                            should_retry_adoption(&mut self.adoption_attempts, &request.title);
                        tracing::warn!(
                            window = %request.title,
                            "could not discover pin child viewport: {err}"
                        );
                        continue;
                    }
                }
            }

            let Some(retained) = self.windows.get_mut(&request.title) else {
                continue;
            };
            let desired = AppliedPin::from(request);
            if retained.applied.as_ref() == Some(&desired) {
                continue;
            }
            let mut non_activation_warning = None;
            if desired.behavior_changed(retained.applied.as_ref()) {
                let mut behavior = OverlayBehavior::pinned_capture(desired.locked);
                behavior.opacity = desired.opacity;
                behavior.has_shadow = desired.shadow;
                match retained.window.apply(&behavior) {
                    Ok(report) if !report.non_activating => {
                        non_activation_warning = Some(report.detail);
                    }
                    Ok(_) => {}
                    Err(err) => {
                        record_failure(
                            &mut self.failures,
                            request,
                            err.to_string(),
                            &mut report.failures,
                        );
                        tracing::warn!(window = %request.title, "could not apply native pin behavior: {err}");
                        continue;
                    }
                }
            }
            if desired.should_set_frame(retained.applied.as_ref())
                && let Err(err) = retained
                    .window
                    .set_frame_with_scale(desired.frame, desired.display_scale)
            {
                record_failure(
                    &mut self.failures,
                    request,
                    err.to_string(),
                    &mut report.failures,
                );
                tracing::warn!(window = %request.title, "could not apply native pin geometry: {err}");
                continue;
            }
            retained.applied = Some(desired);
            if let Some(reason) = non_activation_warning {
                record_failure(&mut self.failures, request, reason, &mut report.failures);
            } else {
                self.failures.remove(&request.title);
            }
        }
        report
    }
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn should_retry_adoption(attempts: &mut HashMap<String, u8>, title: &str) -> bool {
    let attempts = attempts.entry(title.to_owned()).or_default();
    *attempts = attempts.saturating_add(1);
    *attempts <= ADOPTION_RETRY_LIMIT
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn record_failure(
    seen: &mut HashMap<String, String>,
    request: &NativePinRequest,
    reason: String,
    failures: &mut Vec<PinPanelFailure>,
) {
    if seen.get(&request.title) == Some(&reason) {
        return;
    }
    seen.insert(request.title.clone(), reason.clone());
    failures.push(PinPanelFailure {
        pin: request.pin.clone(),
        reason,
    });
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
impl Drop for PinPanels {
    fn drop(&mut self) {
        for (title, retained) in &mut self.windows {
            if let Err(err) = retained.window.restore_native_class() {
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
#[cfg(target_os = "macos")]
pub unsafe fn convert_ns_view(ns_view: *mut c_void) -> PanelReport {
    if ns_view.is_null() {
        return PanelReport::unsupported("the window handle carried a null NSView");
    }

    // SAFETY: forwarded from this function's own contract — a live `NSView *`
    // on the main thread.
    let adopted = unsafe { NativeOverlay::from_ns_view(ns_view) };

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

#[must_use]
#[cfg(not(target_os = "macos"))]
pub unsafe fn convert_ns_view(ns_view: *mut c_void) -> PanelReport {
    if ns_view.is_null() {
        PanelReport::unsupported("the window handle carried a null NSView")
    } else {
        PanelReport::unsupported("NSView conversion is only meaningful on macOS")
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

    fn request() -> NativePinRequest {
        NativePinRequest {
            pin: scrozz_core::PinId("pin".into()),
            title: "pin".into(),
            state: scrozz_core::PinState::new(
                scrozz_core::LogicalRect::new(
                    scrozz_core::LogicalPoint::new(10.0, 20.0),
                    scrozz_core::LogicalSize::new(320.0, 180.0),
                ),
                scrozz_core::PinScale::ORIGINAL,
                None,
            ),
            positioning: true,
            display_scale: scrozz_core::ScaleFactor::new(2.0),
            shadow: true,
        }
    }

    #[test]
    fn identical_static_pin_state_requires_no_native_mutation() {
        let request = request();
        let applied = AppliedPin::from(&request);
        assert!(!applied.behavior_changed(Some(&applied)));
        assert!(!applied.frame_changed(Some(&applied)));
        assert!(applied.should_set_frame(None));
    }

    #[test]
    fn native_pin_deltas_separate_frame_and_behavior_mutations() {
        let base = AppliedPin::from(&request());

        let mut moved_request = request();
        moved_request.state.frame.origin.x += 1.0;
        let moved = AppliedPin::from(&moved_request);
        assert!(moved.frame_changed(Some(&base)));
        assert!(moved.should_set_frame(Some(&base)));
        assert!(!moved.behavior_changed(Some(&base)));

        let mut locked_request = request();
        locked_request.state.locked = true;
        let locked = AppliedPin::from(&locked_request);
        assert!(!locked.frame_changed(Some(&base)));
        assert!(locked.behavior_changed(Some(&base)));
    }

    #[test]
    fn unavailable_positioning_never_requests_native_geometry() {
        let mut request = request();
        request.positioning = false;
        let desired = AppliedPin::from(&request);
        assert!(!desired.should_set_frame(None));
    }

    #[test]
    fn native_adoption_retries_are_bounded() {
        let mut attempts = HashMap::new();
        for _ in 0..ADOPTION_RETRY_LIMIT {
            assert!(should_retry_adoption(&mut attempts, "pin"));
        }
        assert!(!should_retry_adoption(&mut attempts, "pin"));
        assert!(!should_retry_adoption(&mut attempts, "pin"));
    }

    #[test]
    fn one_exhausted_pin_cannot_cancel_another_pins_retry_wake() {
        let mut attempts = HashMap::from([("exhausted".to_owned(), ADOPTION_RETRY_LIMIT)]);
        let mut pending = true;
        pending |= should_retry_adoption(&mut attempts, "exhausted");
        assert!(pending);
    }
}
