//! Adopting eframe's native overlay window without taking ownership from winit.
//!
//! # Why this lives here
//!
//! `scrozz-ui` is `#![forbid(unsafe_code)]` and deliberately does not depend on
//! `scrozz-shell`, so it cannot adopt the native window itself; it exposes a
//! [`PanelHook`] instead. This crate depends on both, so this is the only place
//! the two halves can meet.
//!
//! # Why it takes a pointer
//!
//! [`PanelHook`] is `FnOnce(&eframe::CreationContext<'_>) -> PanelReport`.
//! [`hook`] extracts the native handle and retains a non-owning adapter for safe
//! property mutation while winit remains the sole owner of class, delegate, KVO,
//! close, and release.
//!
//! # Why conversion is forbidden
//!
//! Stable winit 0.30 cannot construct its macOS window as an `NSPanel` through
//! eframe. Runtime `isa` mutation is not an alternative: it disconnects winit
//! and AppKit KVO registrations and crashes later during delegate/view teardown.
//! The adapter therefore reports the non-activating capability as unavailable
//! while preserving every other overlay behavior.

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use std::collections::{HashMap, HashSet};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    rc::Rc,
};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use scrozz_core::LogicalRect;
use scrozz_shell::OverlayWindow;
use scrozz_shell::{NativeOverlay, NativeSurface, OverlayBehavior, OverlayCursor};
use scrozz_ui::{PanelReport, PanelSetup, recent_captures_overlay::NativePinRequest};

#[derive(Clone, Debug, PartialEq)]
struct AppliedPin {
    frame: scrozz_core::LogicalRect,
    positioning: bool,
    display_scale: scrozz_core::ScaleFactor,
    locked: bool,
    passthrough: bool,
    shadow: bool,
}

impl From<&NativePinRequest> for AppliedPin {
    fn from(request: &NativePinRequest) -> Self {
        Self {
            frame: request.state.frame,
            positioning: request.positioning,
            display_scale: request.display_scale,
            locked: request.state.locked,
            passthrough: request.passthrough,
            shadow: request.shadow,
        }
    }
}

impl AppliedPin {
    fn behavior_changed(&self, previous: Option<&Self>) -> bool {
        previous.is_none_or(|previous| {
            self.locked != previous.locked
                || self.passthrough != previous.passthrough
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

    /// Restores every adopted child before eframe drops its winit viewports.
    pub fn prepare_for_winit_teardown(&mut self) {
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        for (title, mut retained) in self.windows.drain() {
            if let Err(err) = retained.window.restore_native_class() {
                tracing::warn!(window = %title, "could not return pin window to its owner: {err}");
            }
        }
        #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
        {
            self.pending_logged.clear();
            self.failures.clear();
            self.adoption_attempts.clear();
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
            if desired.behavior_changed(retained.applied.as_ref()) {
                let mut behavior =
                    OverlayBehavior::pinned_capture(desired.locked, desired.passthrough);
                behavior.has_shadow = desired.shadow;
                match retained.window.apply(&behavior) {
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
            self.failures.remove(&request.title);
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
        self.prepare_for_winit_teardown();
    }
}

/// The raw native window a drag can be begun from, if this platform has one.
///
/// Every backend wants a different handle — an `NSView *`, an `HWND`, an X11
/// window id widened to a pointer — and the one place that knows which is the
/// one that already matched on the raw handle. Wayland has no such handle at
/// all: drags there travel over `wl_data_device` and need the compositor
/// protocol object, not a window, so this answers `None` rather than inventing
/// a pointer that would be dereferenced.
///
/// # Safety
///
/// `handle` must name a live window, and the returned surface must not outlive
/// it.
unsafe fn native_surface_of(handle: RawWindowHandle) -> Option<NativeSurface> {
    let raw: *mut c_void = match handle {
        #[cfg(target_os = "macos")]
        RawWindowHandle::AppKit(appkit) => appkit.ns_view.as_ptr(),
        #[cfg(target_os = "windows")]
        RawWindowHandle::Win32(win32) => isize::from(win32.hwnd) as *mut c_void,
        #[cfg(target_os = "linux")]
        RawWindowHandle::Xlib(xlib) => xlib.window as *mut c_void,
        #[cfg(target_os = "linux")]
        RawWindowHandle::Xcb(xcb) => xcb.window.get() as usize as *mut c_void,
        _ => return None,
    };
    // SAFETY: forwarded from this function's own contract.
    Some(unsafe { NativeSurface::from_raw(raw) })
}

/// Main-thread control of the native window behavior after the creation hook.
///
/// The handle is intentionally `Rc`, not `Arc`: native window mutation belongs
/// to the one eframe thread and must never become callable from a capture worker.
#[derive(Clone, Default)]
pub struct BehaviorController {
    /// The raw native window, kept so a drag can be begun from it.
    ///
    /// Separate from `overlay` because it exists on every platform and is
    /// needed even where no native overlay adapter does: a drag needs the
    /// `NSView *` / `HWND` / X11 window itself, not the panel behaviour built
    /// on top of it.
    surface: Rc<RefCell<Option<NativeSurface>>>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    overlay: Rc<RefCell<Option<NativeOverlay>>>,
    #[cfg(target_os = "linux")]
    x11_focus: Rc<RefCell<Option<scrozz_shell::X11FocusLease>>>,
    teardown_started: Rc<Cell<bool>>,
    #[cfg(test)]
    behavior_log: Rc<RefCell<Vec<OverlayBehavior>>>,
    #[cfg(test)]
    cursor_log: Rc<RefCell<Vec<OverlayCursor>>>,
    #[cfg(test)]
    visibility_log: Rc<RefCell<Vec<bool>>>,
    #[cfg(test)]
    action_log: Rc<RefCell<Vec<RecordedNativeAction>>>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RecordedNativeAction {
    Frame(LogicalRect),
    Behavior(OverlayBehavior),
    Cursor(OverlayCursor),
    Visible(bool),
    RestoreSuppressedWindows,
    ReturnToOwner,
}

impl BehaviorController {
    /// Re-anchors the retained native window to its exact transparent viewport.
    ///
    /// Eframe's viewport position is only a window-manager hint. The native
    /// frame is authoritative on macOS and keeps cards above the Dock while
    /// retaining room below them for shadows.
    pub fn set_frame(&self, frame: LogicalRect) {
        if self.teardown_started.get() {
            return;
        }
        #[cfg(target_os = "macos")]
        if let Some(overlay) = self.overlay.borrow_mut().as_mut()
            && let Err(error) = overlay.set_frame(frame)
        {
            tracing::warn!(%error, "could not re-anchor native overlay frame");
        }

        #[cfg(not(target_os = "macos"))]
        let _ = frame;

        #[cfg(test)]
        self.action_log
            .borrow_mut()
            .push(RecordedNativeAction::Frame(frame));
    }

    /// Applies one click-through transition and reports what the window says.
    ///
    /// # Errors
    ///
    /// Returns the platform's own message when the transition or the readback
    /// was refused.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn apply_click_through(&self, requested: bool) -> Result<bool, String> {
        if self.teardown_started.get() {
            return Ok(false);
        }
        let mut slot = self.overlay.borrow_mut();
        let Some(overlay) = slot.as_mut() else {
            // No retained adapter is not an acknowledgement. Automatic
            // scrolling stays blocked rather than scrolling this overlay.
            return Ok(false);
        };
        overlay
            .set_click_through(requested)
            .map_err(|error| error.to_string())?;
        #[cfg(target_os = "macos")]
        {
            overlay
                .click_through()
                .map(|actual| actual == requested)
                .map_err(|error| error.to_string())
        }
        #[cfg(not(target_os = "macos"))]
        {
            // Windows has no readback API on the adapter; the style write above
            // either returned an error or took effect.
            Ok(true)
        }
    }

    /// Applies a card or selection behavior when a native adapter is retained.
    pub fn apply(&self, behavior: &OverlayBehavior) {
        if self.teardown_started.get() {
            return;
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
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
        {
            self.behavior_log.borrow_mut().push(*behavior);
            self.action_log
                .borrow_mut()
                .push(RecordedNativeAction::Behavior(*behavior));
        }

        if behavior.click_through {
            self.set_cursor(OverlayCursor::Arrow);
        }

        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux", test)))]
        let _ = behavior;
    }

    /// Makes the native cursor stick for as long as this choice holds, rather
    /// than leaving it to whichever cursor winit's own tracking last set.
    ///
    /// Routes through the installed [`scrozz_shell::macos::overlay::MacOverlay`]
    /// when one exists: only its `set_cursor` can pin *that window's* cursor
    /// rect, which is what stops a direct `NSCursor::set()` from being quietly
    /// reverted by the next native mouse-moved event (see that method's docs).
    /// The free function is a best-effort fallback for the moment before the
    /// panel hook installs an overlay at all.
    pub fn set_cursor(&self, cursor: OverlayCursor) {
        if self.teardown_started.get() {
            return;
        }
        #[cfg(all(target_os = "macos", not(test)))]
        {
            let routed = self
                .overlay
                .borrow()
                .as_ref()
                .map(|overlay| overlay.set_cursor(cursor));
            let result =
                routed.unwrap_or_else(|| scrozz_shell::macos::overlay::set_overlay_cursor(cursor));
            if let Err(error) = result {
                tracing::warn!(%error, "could not update native overlay cursor");
            }
        }

        #[cfg(test)]
        {
            self.cursor_log.borrow_mut().push(cursor);
            self.action_log
                .borrow_mut()
                .push(RecordedNativeAction::Cursor(cursor));
        }

        #[cfg(all(not(target_os = "macos"), not(test)))]
        let _ = cursor;
    }

    /// Orders the retained utility surface in or out immediately.
    ///
    /// Viewport commands are still sent alongside this by the host for winit's
    /// bookkeeping. The native call is what makes the terminal lifecycle
    /// synchronous from the window server's point of view.
    pub fn set_visible(&self, visible: bool) {
        if self.teardown_started.get() {
            return;
        }
        #[cfg(target_os = "macos")]
        if let Some(overlay) = self.overlay.borrow_mut().as_mut()
            && let Err(error) = overlay.set_visible(visible)
        {
            tracing::warn!(%error, visible, "could not change native overlay visibility");
        }

        #[cfg(test)]
        {
            self.visibility_log.borrow_mut().push(visible);
            self.action_log
                .borrow_mut()
                .push(RecordedNativeAction::Visible(visible));
        }

        #[cfg(all(not(target_os = "macos"), not(test)))]
        let _ = visible;
    }

    /// Restores ordinary windows that selector activation temporarily ordered
    /// out, after the capture backend has finished acquiring pixels.
    pub fn restore_suppressed_windows(&self) {
        if self.teardown_started.get() {
            return;
        }
        #[cfg(target_os = "macos")]
        if let Some(overlay) = self.overlay.borrow_mut().as_mut()
            && let Err(error) = overlay.restore_suppressed_windows()
        {
            tracing::warn!(%error, "could not restore windows suppressed during capture");
        }
        #[cfg(test)]
        self.action_log
            .borrow_mut()
            .push(RecordedNativeAction::RestoreSuppressedWindows);
    }

    /// Orders out ordinary application windows without activating the app.
    pub fn suppress_auxiliary_windows(&self) {
        if self.teardown_started.get() {
            return;
        }
        #[cfg(target_os = "macos")]
        if let Some(overlay) = self.overlay.borrow_mut().as_mut()
            && let Err(error) = overlay.suppress_auxiliary_windows()
        {
            tracing::warn!(%error, "could not suppress auxiliary windows during capture");
        }
    }

    /// Keeps ordinary child windows ordered out while selector activation owns
    /// the application, even if their viewport builders are serviced meanwhile.
    pub fn keep_suppressed_windows_hidden(&self) {
        if self.teardown_started.get() {
            return;
        }
        #[cfg(target_os = "macos")]
        if let Some(overlay) = self.overlay.borrow_mut().as_mut()
            && let Err(error) = overlay.keep_suppressed_windows_hidden()
        {
            tracing::warn!(%error, "could not keep auxiliary windows suppressed");
        }
    }

    /// Retries native behavior that had to wait for queued window commands.
    pub fn refresh(&self) {
        #[cfg(target_os = "linux")]
        if !self.teardown_started.get()
            && let Some(focus) = self.x11_focus.borrow_mut().as_mut()
            && let Err(error) = focus.refresh()
        {
            tracing::warn!(%error, "could not acquire X11 selector keyboard focus");
        }
    }

    /// The native window drags start from, once the window exists.
    #[must_use]
    pub fn native_surface(&self) -> Option<NativeSurface> {
        if self.teardown_started.get() {
            return None;
        }
        *self.surface.borrow()
    }

    /// Stops native mutation and returns the root window to winit exactly once.
    pub fn prepare_for_winit_teardown(&self) -> scrozz_core::Result<()> {
        if self.teardown_started.replace(true) {
            return Ok(());
        }
        self.surface.borrow_mut().take();

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(mut overlay) = self.overlay.borrow_mut().take() {
            overlay.restore_native_class()?;
        }

        #[cfg(target_os = "linux")]
        self.x11_focus.borrow_mut().take();

        #[cfg(test)]
        self.action_log
            .borrow_mut()
            .push(RecordedNativeAction::ReturnToOwner);

        Ok(())
    }

    /// Records the native window, as reported by the creation hook.
    fn install_surface(&self, surface: NativeSurface) {
        if self.teardown_started.get() {
            return;
        }
        *self.surface.borrow_mut() = Some(surface);
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn install(&self, overlay: NativeOverlay) {
        if self.teardown_started.get() {
            return;
        }
        *self.overlay.borrow_mut() = Some(overlay);
    }

    #[cfg(target_os = "linux")]
    fn install_x11_focus(&self, focus: scrozz_shell::X11FocusLease) {
        if self.teardown_started.get() {
            return;
        }
        *self.x11_focus.borrow_mut() = Some(focus);
    }

    #[cfg(test)]
    pub(crate) fn recording() -> (Self, Rc<RefCell<Vec<OverlayBehavior>>>) {
        let controller = Self::default();
        (controller.clone(), Rc::clone(&controller.behavior_log))
    }

    #[cfg(test)]
    pub(crate) fn recorded_cursors(&self) -> Vec<OverlayCursor> {
        self.cursor_log.borrow().clone()
    }

    #[cfg(test)]
    pub(crate) fn recorded_visibility(&self) -> Vec<bool> {
        self.visibility_log.borrow().clone()
    }

    #[cfg(test)]
    pub(crate) fn recorded_actions(&self) -> Vec<RecordedNativeAction> {
        self.action_log.borrow().clone()
    }
}

/// Adopts the window hosting `ns_view` without changing its runtime identity.
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
pub unsafe fn adopt_ns_view(ns_view: *mut c_void) -> PanelReport {
    // SAFETY: forwarded from this function's own contract.
    unsafe { adopt_and_retain_ns_view(ns_view) }.0
}

#[cfg(target_os = "macos")]
unsafe fn adopt_and_retain_ns_view(ns_view: *mut c_void) -> (PanelReport, Option<NativeOverlay>) {
    if ns_view.is_null() {
        return (
            PanelReport::unsupported("the window handle carried a null NSView"),
            None,
        );
    }

    // SAFETY: forwarded from this function's own contract — a live `NSView *`
    // on the main thread.
    let mut overlay = match unsafe { NativeOverlay::from_ns_view(ns_view) } {
        Ok(overlay) => overlay,
        Err(err) => return (PanelReport::unsupported(err.to_string()), None),
    };

    let report = match overlay.apply(&OverlayBehavior::capture_card()) {
        Ok(report) if report.non_activating => PanelReport::converted(report.detail),
        Ok(report) => PanelReport::unsupported(report.detail),
        Err(err) => PanelReport::unsupported(err.to_string()),
    };
    if let Err(error) = overlay.set_visible(false) {
        tracing::warn!(%error, "could not order the initial overlay out");
    }
    (report, Some(overlay))
}

/// Adopts the Win32 overlay window and keeps the adapter alive.
///
/// The adapter restores every style it set when it is dropped, so the retained
/// value — not the call — is what keeps the overlay non-activating, topmost,
/// and excluded from display capture (`WDA_EXCLUDEFROMCAPTURE`).
///
/// # Safety
///
/// `hwnd` must name the live top-level window this process just created.
#[cfg(target_os = "windows")]
unsafe fn adopt_and_retain_hwnd(hwnd: *mut c_void) -> (PanelReport, Option<NativeOverlay>) {
    // SAFETY: forwarded from this function's own contract.
    let adopted = unsafe { NativeOverlay::from_hwnd(hwnd) };
    let mut overlay = match adopted {
        Ok(overlay) => overlay,
        Err(err) => return (PanelReport::unsupported(err.to_string()), None),
    };
    let report = match overlay.apply(&OverlayBehavior::capture_card()) {
        Ok(report) if report.non_activating => PanelReport::converted(report.detail),
        Ok(report) => PanelReport::unsupported(report.detail),
        Err(err) => return (PanelReport::unsupported(err.to_string()), None),
    };
    (report, Some(overlay))
}

#[cfg(not(target_os = "macos"))]
unsafe fn adopt_and_retain_ns_view(ns_view: *mut c_void) -> (PanelReport, Option<NativeOverlay>) {
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

#[must_use]
#[cfg(not(target_os = "macos"))]
pub unsafe fn adopt_ns_view(ns_view: *mut c_void) -> PanelReport {
    if ns_view.is_null() {
        PanelReport::unsupported("the window handle carried a null NSView")
    } else {
        PanelReport::unsupported("NSView conversion is only meaningful on macOS")
    }
}

/// The hook `scrozz-ui` runs while the overlay window is being created.
///
/// Extracts the platform handle from the `CreationContext` and hands it to
/// [`adopt_ns_view`]. That is the whole hook: everything with platform risk in
/// it lives in the adoption, which is unit-tested without a window.
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
                return PanelSetup::unsupported(format!("eframe reported no window handle: {err}"));
            }
        };

        // SAFETY: the raw handle names a live window for the duration of this
        // hook, and the surface is only ever used from this same main thread
        // while the window is open.
        if let Some(native) = unsafe { native_surface_of(handle.as_raw()) } {
            controller.install_surface(native);
        }

        #[cfg(target_os = "macos")]
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return PanelSetup::unsupported(
                "the overlay window is not an AppKit window, so it has no NSView to convert",
            );
        };

        // SAFETY: `handle` borrows the window for this scope, so the view is
        // alive; `RecentCapturesOverlayApp::new` runs on the main thread.
        #[cfg(target_os = "macos")]
        {
            let (report, overlay) =
                // SAFETY: the handle borrow keeps the NSView alive for adoption.
                unsafe { adopt_and_retain_ns_view(appkit.ns_view.as_ptr()) };
            if let Some(overlay) = overlay {
                controller.install(overlay);
            }
            // The retained adapter is the click-through controller: automatic
            // scrolling needs an *acknowledged* transition, and only the native
            // window can answer that.
            PanelSetup::new(report).with_passthrough(passthrough_controller(&controller))
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
            let setup = PanelSetup::new(PanelReport::unsupported(detail));
            match handle.as_raw() {
                RawWindowHandle::Xlib(xlib) => u32::try_from(xlib.window).map_or(setup, |window| {
                    setup.with_passthrough(x11_passthrough(window))
                }),
                RawWindowHandle::Xcb(xcb) => {
                    setup.with_passthrough(x11_passthrough(xcb.window.get()))
                }
                // Wayland automatic scrolling is manual-only, so there is no
                // click-through contract to honour and none is invented.
                _ => setup,
            }
        }

        #[cfg(target_os = "windows")]
        {
            let RawWindowHandle::Win32(win32) = handle.as_raw() else {
                return PanelSetup::unsupported(
                    "the overlay window is not a Win32 window, so it cannot be excluded from capture",
                );
            };
            let (report, overlay) =
                // SAFETY: the creation-context handle borrows this live top-level window.
                unsafe { adopt_and_retain_hwnd(win32.hwnd.get() as *mut c_void) };
            if let Some(overlay) = overlay {
                controller.install(overlay);
            }
            PanelSetup::new(report).with_passthrough(passthrough_controller(&controller))
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            let _ = handle;
            let _ = controller;
            PanelSetup::unsupported(
                "only the macOS overlay backend is implemented so far, so the \
                 window keeps its native activation behaviour",
            )
        }
    })
}

/// A click-through controller backed by the retained native overlay adapter.
///
/// Returns whether the window *reports* the requested state, not whether the
/// call was made: automatic scrolling posts globally addressed wheel input, and
/// an unacknowledged request would send it into Scrozz's own overlay.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn passthrough_controller(controller: &BehaviorController) -> scrozz_ui::NativePassthrough {
    let controller = controller.clone();
    Box::new(move |requested| controller.apply_click_through(requested))
}

/// The X11 click-through controller, expressed as an empty input shape.
///
/// `scrozz-shell`'s X11 overlay deliberately refuses pointer transparency for
/// pins, because a pin has no focus-release contract. A scrolling capture does:
/// it is a bounded session that restores the input region as soon as it ends,
/// so the shape is set here rather than by relaxing that refusal.
#[cfg(target_os = "linux")]
fn x11_passthrough(window: u32) -> scrozz_ui::NativePassthrough {
    use x11rb::{
        connection::Connection as _,
        protocol::{
            shape::{ConnectionExt as _, SK, SO},
            xproto::{ClipOrdering, ConnectionExt as _, Rectangle},
        },
    };

    let mut applied = false;
    Box::new(move |requested| {
        let (connection, _) = x11rb::connect(None)
            .map_err(|error| format!("the X11 overlay connection failed: {error}"))?;
        let rectangles = if requested {
            Vec::new()
        } else {
            let geometry = connection
                .get_geometry(window)
                .map_err(|error| error.to_string())?
                .reply()
                .map_err(|error| error.to_string())?;
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
            .map_err(|error| error.to_string())?;
        connection
            .flush()
            .map_err(|error| format!("the X11 input shape could not be flushed: {error}"))?;
        applied = requested;
        Ok(applied == requested)
    })
}

/// Attaches direct keyboard-focus ownership to an eframe X11 window.
///
/// This is separate from native AppKit adoption so the one-shot selector can
/// retain X11 focus.
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
        let report = unsafe { adopt_ns_view(std::ptr::null_mut()) };
        assert!(!report.non_activating);
        assert!(report.detail.contains("null"), "{}", report.detail);
    }

    #[test]
    fn refusal_is_reported_not_raised() {
        // The property the whole hook design rests on: a hook that could fail
        // is a hook that can take down the window it was called to configure.
        // Every path returns a report, so the overlay always survives.
        let report = unsafe { adopt_ns_view(std::ptr::null_mut()) };
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

    #[test]
    fn returning_a_window_to_winit_is_idempotent_and_terminal() {
        let controller = BehaviorController::default();
        controller.apply(&OverlayBehavior::capture_card());
        controller
            .prepare_for_winit_teardown()
            .expect("the mock controller has no native failure");
        controller
            .prepare_for_winit_teardown()
            .expect("a repeated teardown is a no-op");
        controller.set_visible(true);
        controller.apply(&OverlayBehavior::selection_overlay());

        assert_eq!(
            controller.recorded_actions(),
            vec![
                RecordedNativeAction::Behavior(OverlayBehavior::capture_card()),
                RecordedNativeAction::ReturnToOwner,
            ],
            "no native mutation may follow the one return-to-owner boundary"
        );
        assert!(controller.native_surface().is_none());
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
            passthrough: false,
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

        let mut control_island_request = locked_request;
        control_island_request.passthrough = false;
        let control_island = AppliedPin::from(&control_island_request);
        let mut click_through_request = control_island_request;
        click_through_request.passthrough = true;
        let click_through = AppliedPin::from(&click_through_request);
        assert!(click_through.locked);
        assert!(click_through.behavior_changed(Some(&control_island)));
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
