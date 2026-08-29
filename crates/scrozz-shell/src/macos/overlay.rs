//! Native mutation of winit-owned macOS windows.
//!
//! Winit 0.30 registers KVO observers on each `WinitWindow`. AppKit may add
//! further observers later. Changing that object's runtime class with
//! `object_setClass` severs those registrations and makes delegate or view
//! teardown throw an uncaught `NSRangeException`. Scrozz therefore preserves
//! the exact native class and delegate for the complete winit-owned lifetime.
//!
//! Stable winit does not yet expose its native `NSPanel` constructor through
//! eframe, so these windows keep ordinary activation behavior for now. All
//! other supported native properties remain available.

use std::ffi::c_void;

use objc2::ClassType;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationPresentationOptions, NSCursor,
    NSFloatingWindowLevel, NSNormalWindowLevel, NSPanel, NSPopUpMenuWindowLevel,
    NSRunningApplication, NSScreenSaverWindowLevel, NSStatusWindowLevel, NSView, NSWindow,
    NSWindowAnimationBehavior, NSWindowCollectionBehavior, NSWindowLevel, NSWindowSharingType,
    NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::NSRect;
use scrozz_core::{Error, LogicalRect, Result};

use crate::OverlayWindow;
use crate::macos::{display, main_thread};
use crate::overlay::{
    AppKitRect, OverlayBehavior, OverlayCursor, OverlayLevel, OverlayReport, appkit_to_logical,
    logical_to_appkit,
};

/// Resolves an [`OverlayLevel`] to its `NSWindowLevel`.
///
#[must_use]
pub fn level_value(level: OverlayLevel) -> NSWindowLevel {
    match level {
        OverlayLevel::Normal => NSNormalWindowLevel,
        OverlayLevel::Floating => NSFloatingWindowLevel,
        OverlayLevel::Status => NSStatusWindowLevel,
        OverlayLevel::AboveMenuBar => NSPopUpMenuWindowLevel,
        OverlayLevel::ScreenSaver => NSScreenSaverWindowLevel,
    }
}

/// Immediately changes the native cursor while Scrozz owns pointer input.
///
/// Winit normally applies egui's cursor output. A retained capture-card window
/// can expand underneath a stationary pointer, however, while winit still
/// considers that pointer outside the old card-sized frame. AppKit must receive
/// the first cursor directly; normal cursor-rect handling resumes as soon as a
/// pointer event reaches winit.
///
/// This free function has no window to hold onto, so it cannot do anything
/// about winit's *own* cursor rect on whichever window is under the pointer —
/// see [`MacOverlay::set_cursor`] for why that matters and for the version that
/// actually keeps a chosen cursor from being silently reverted. Prefer that
/// method whenever an installed [`MacOverlay`] is available; this function
/// exists for the moments before one is, or for a window this crate never
/// retrofitted (a secondary-display selection viewport, say).
///
/// # Errors
///
/// Returns [`Error::Platform`] when called off the main thread.
pub fn set_overlay_cursor(cursor: OverlayCursor) -> Result<()> {
    let _mtm = main_thread("setting the overlay cursor")?;
    match cursor {
        OverlayCursor::Arrow => NSCursor::arrowCursor(),
        OverlayCursor::Crosshair => NSCursor::crosshairCursor(),
    }
    .set();
    Ok(())
}

/// Assembles the `NSWindowCollectionBehavior` mask for a behaviour.
///
/// `CanJoinAllSpaces` is what keeps an overlay alive across a Space switch —
/// without it, a capture card taken on one Space simply vanishes when the user
/// swipes. `FullScreenAuxiliary` is what lets it appear over a fullscreen app,
/// which is where users spend most of their time on a laptop.
#[must_use]
pub fn collection_behavior(behavior: &OverlayBehavior) -> NSWindowCollectionBehavior {
    let mut mask = NSWindowCollectionBehavior::empty();
    if behavior.join_all_spaces {
        mask |= NSWindowCollectionBehavior::CanJoinAllSpaces;
    }
    if behavior.over_fullscreen {
        mask |= NSWindowCollectionBehavior::FullScreenAuxiliary;
    }
    if behavior.stationary {
        mask |= NSWindowCollectionBehavior::Stationary;
    }
    if behavior.ignore_cycle {
        mask |= NSWindowCollectionBehavior::IgnoresCycle;
    }
    mask
}

/// A native macOS overlay: an `NSWindow` someone else created and still owns.
///
/// Holds a strong reference to the window, so it is safe to keep across frames,
/// but does **not** own it: dropping a `MacOverlay` neither closes nor releases
/// the underlying window beyond its own retain.
#[derive(Debug)]
pub struct MacOverlay {
    window: Retained<NSWindow>,
    presentation_lease: Option<PresentationLease>,
}

#[derive(Debug)]
struct PresentationLease {
    previous_application: Option<Retained<NSRunningApplication>>,
    previous_options: NSApplicationPresentationOptions,
}

impl PresentationLease {
    fn acquire(mtm: objc2::MainThreadMarker) -> Self {
        let app = NSApplication::sharedApplication(mtm);
        let previous_options = app.presentationOptions();
        let previous_application = NSWorkspace::sharedWorkspace().frontmostApplication();
        let mut selection_options = previous_options;
        selection_options.remove(NSApplicationPresentationOptions::AutoHideDock);
        selection_options.insert(NSApplicationPresentationOptions::HideDock);
        app.setPresentationOptions(selection_options);
        app.activate();
        Self {
            previous_application,
            previous_options,
        }
    }

    fn release(self, mtm: objc2::MainThreadMarker) {
        let app = NSApplication::sharedApplication(mtm);
        app.setPresentationOptions(self.previous_options);
        app.deactivate();
        if let Some(previous) = self.previous_application
            && !previous.isTerminated()
        {
            let _ = previous.activateWithOptions(NSApplicationActivationOptions::empty());
        }
    }
}

impl MacOverlay {
    /// Finds and adopts one of this process's windows by its exact title.
    ///
    /// eframe exposes native handles only for the root viewport. Pinned
    /// captures are child viewports, so their stable private titles are the
    /// bridge that lets the app retain and configure each child without
    /// changing winit's class or delegate.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] off the main thread.
    pub fn find_by_title(title: &str) -> Result<Option<Self>> {
        let mtm = main_thread("finding a pinned overlay window")?;
        let application = NSApplication::sharedApplication(mtm);
        for window in application.windows().iter() {
            if window.title().to_string() != title {
                continue;
            }
            return Ok(Some(Self {
                window,
                presentation_lease: None,
            }));
        }
        Ok(None)
    }

    /// Adopts the window hosting an `NSView`.
    ///
    /// This is the path from `raw-window-handle`: eframe reports
    /// `RawWindowHandle::AppKit { ns_view }`, and the window is
    /// `[ns_view window]`. Passing the view rather than the window means
    /// `scrozz-ui` never has to name AppKit.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for a null pointer, [`Error::Platform`]
    /// off the main thread, and [`Error::TargetGone`] if the view is not in a
    /// window — which happens if the handle outlived its window.
    ///
    /// # Safety
    ///
    /// `ns_view` must be a live `NSView *`, as obtained from a
    /// `RawWindowHandle::AppKit` that has not been dropped.
    pub unsafe fn from_ns_view(ns_view: *mut c_void) -> Result<Self> {
        let _mtm = main_thread("adopting an overlay window")?;
        if ns_view.is_null() {
            return Err(Error::InvalidRequest(
                "null NSView pointer passed to MacOverlay::from_ns_view".to_owned(),
            ));
        }
        // SAFETY: the caller guarantees a live `NSView *`, and the reference
        // does not outlive this function — `window()` retains its result.
        let view: &NSView = unsafe { &*ns_view.cast::<NSView>() };
        let window = view.window().ok_or_else(|| {
            Error::TargetGone("the NSView is not attached to a window".to_owned())
        })?;
        Ok(Self {
            window,
            presentation_lease: None,
        })
    }

    /// Adopts an `NSWindow` directly.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for a null pointer and
    /// [`Error::Platform`] off the main thread.
    ///
    /// # Safety
    ///
    /// `ns_window` must be a live `NSWindow *`.
    pub unsafe fn from_ns_window(ns_window: *mut c_void) -> Result<Self> {
        let _mtm = main_thread("adopting an overlay window")?;
        if ns_window.is_null() {
            return Err(Error::InvalidRequest(
                "null NSWindow pointer passed to MacOverlay::from_ns_window".to_owned(),
            ));
        }
        // SAFETY: the caller guarantees a live `NSWindow *`; `retain` gives us
        // an owned reference with the usual ARC semantics.
        let window =
            unsafe { Retained::retain(ns_window.cast::<NSWindow>()) }.ok_or_else(|| {
                Error::TargetGone("the NSWindow pointer could not be retained".to_owned())
            })?;
        Ok(Self {
            window,
            presentation_lease: None,
        })
    }

    /// The underlying window, for callers already inside AppKit.
    #[must_use]
    pub fn window(&self) -> &NSWindow {
        &self.window
    }

    /// Applies a complete overlay behaviour without changing winit's class or
    /// delegate.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if called off the main thread.
    pub fn apply(&mut self, behavior: &OverlayBehavior) -> Result<OverlayReport> {
        let mtm = main_thread("configuring an overlay window")?;

        if behavior.suppress_system_ui {
            if self.presentation_lease.is_none() {
                self.presentation_lease = Some(PresentationLease::acquire(mtm));
            }
        } else if let Some(lease) = self.presentation_lease.take() {
            lease.release(mtm);
        }

        let window = &self.window;
        let release_key_focus = behavior.click_through && window.isKeyWindow();
        window.setLevel(level_value(behavior.level));
        window.setCollectionBehavior(collection_behavior(behavior));
        window.setHidesOnDeactivate(behavior.hides_on_deactivate);
        window.setIgnoresMouseEvents(behavior.click_through);
        window.setAcceptsMouseMovedEvents(!behavior.click_through);
        window.setOpaque(behavior.opaque);
        window.setHasShadow(behavior.has_shadow);
        window.setAlphaValue(behavior.opacity.get());
        window.setSharingType(if behavior.capture_excluded {
            NSWindowSharingType::None
        } else {
            NSWindowSharingType::ReadOnly
        });
        window.setMovable(behavior.movable);
        window.setMovableByWindowBackground(false);

        // If a future winit/eframe release constructs a native NSPanel, this
        // keeps click focus separate from controls that genuinely need typing.
        if let Some(panel) = window.downcast_ref::<NSPanel>() {
            panel.setBecomesKeyOnlyIfNeeded(!behavior.accepts_key);
            panel.setFloatingPanel(behavior.level >= OverlayLevel::Floating);
        }

        if release_key_focus {
            // A locked pin must not keep swallowing keys after its pointer input
            // becomes transparent. Ordering it out relinquishes key status;
            // ordering it straight back leaves focus ownership with AppKit.
            window.orderOut(None);
            window.orderFrontRegardless();
        }

        let non_activating = window.isKindOfClass(NSPanel::class())
            && window
                .styleMask()
                .contains(NSWindowStyleMask::NonactivatingPanel);
        Ok(OverlayReport {
            non_activating,
            detail: if non_activating {
                "using the native panel class supplied by the window owner".to_owned()
            } else {
                "kept the winit-owned runtime class unchanged; stable winit 0.30 cannot \
                 construct an NSPanel through eframe"
                    .to_owned()
            },
        })
    }

    /// Stops native mutation before returning the window to winit.
    ///
    /// Winit removes its KVO observers while its delegate is deallocated. Scrozz
    /// therefore orders the surface out, restores cursor ownership, and releases
    /// presentation state without changing the runtime class or delegate.
    pub fn restore_native_class(&mut self) -> Result<()> {
        let mtm = main_thread("returning an overlay window to winit")?;
        self.window
            .setAnimationBehavior(NSWindowAnimationBehavior::None);
        self.window.setIgnoresMouseEvents(true);
        self.window.setAcceptsMouseMovedEvents(false);
        if !self.window.areCursorRectsEnabled() {
            self.window.enableCursorRects();
        }
        self.window.orderOut(None);
        if let Some(lease) = self.presentation_lease.take() {
            lease.release(mtm);
        }
        Ok(())
    }

    /// Orders the native surface in or out immediately.
    ///
    /// `orderOut:` is the correctness boundary for idle utility windows.
    /// Transparency and click-through affect pixels and input, but both still
    /// leave a real window in CoreGraphics/ScreenCaptureKit enumeration.
    ///
    /// Hiding is defensive: it also releases pointer/cursor ownership and any
    /// selection presentation lease before returning.
    ///
    /// The native regression below checks the window server, not just AppKit's
    /// local `isVisible` flag. The fixture is a one-point borderless utility
    /// window and is ordered out again immediately.
    ///
    /// ```
    /// use objc2::{MainThreadMarker, MainThreadOnly};
    /// use objc2_app_kit::{
    ///     NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow,
    ///     NSWindowSharingType, NSWindowStyleMask,
    /// };
    /// use objc2_core_graphics::{
    ///     CGWindowID, CGWindowListCreate, CGWindowListOption, kCGNullWindowID,
    /// };
    /// use objc2_foundation::{NSDate, NSPoint, NSRect, NSRunLoop, NSSize};
    /// use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};
    /// use scrozz_shell::OverlayWindow;
    /// use scrozz_shell::macos::overlay::MacOverlay;
    /// use scrozz_shell::overlay::OverlayBehavior;
    ///
    /// let mtm = MainThreadMarker::new().expect("doctests run on the main thread");
    /// let app = NSApplication::sharedApplication(mtm);
    /// let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    /// app.finishLaunching();
    /// let window = unsafe {
    ///     NSWindow::initWithContentRect_styleMask_backing_defer(
    ///         NSWindow::alloc(mtm),
    ///         NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0)),
    ///         NSWindowStyleMask::Borderless,
    ///         NSBackingStoreType::Buffered,
    ///         false,
    ///     )
    /// };
    /// unsafe { window.setReleasedWhenClosed(false) };
    /// let id = CGWindowID::try_from(window.windowNumber()).expect("positive window number");
    /// let flush_window_server = || {
    ///     NSRunLoop::mainRunLoop()
    ///         .runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.01));
    /// };
    /// let is_on_screen = || {
    ///     CGWindowListCreate(
    ///         CGWindowListOption::OptionOnScreenOnly,
    ///         kCGNullWindowID,
    ///     )
    ///     .is_some_and(|list| {
    ///         (0..list.count()).any(|index| {
    ///             let slot = unsafe { list.value_at_index(index) };
    ///             CGWindowID::try_from(slot.addr()).ok() == Some(id)
    ///         })
    ///     })
    /// };
    ///
    /// let mut overlay = unsafe {
    ///     MacOverlay::from_ns_window(
    ///         objc2::rc::Retained::as_ptr(&window).cast_mut().cast(),
    ///     )
    /// }
    /// .expect("adopt fixture");
    /// overlay
    ///     .apply(&OverlayBehavior::capture_card())
    ///     .expect("configure fixture");
    /// overlay.set_visible(false).expect("order out");
    /// assert!(!is_on_screen(), "an idle overlay remained in CGWindowList");
    ///
    /// let target = LogicalRect::new(
    ///     LogicalPoint::new(40.0, 60.0),
    ///     LogicalSize::new(320.0, 240.0),
    /// );
    /// overlay.set_frame(target).expect("prepare native frame");
    /// let prepared = overlay.diagnostics().expect("prepared diagnostics");
    /// assert_eq!(prepared.frame, target);
    /// assert!(prepared.backing_scale > 0.0);
    /// assert!(!prepared.is_visible, "preparation must stay ordered out");
    /// assert!(!is_on_screen(), "prepared frame leaked before first paint");
    ///
    /// overlay
    ///     .apply(&OverlayBehavior::capture_card())
    ///     .expect("install card input before reveal");
    /// overlay.set_visible(true).expect("order front");
    /// flush_window_server();
    /// assert!(is_on_screen(), "the active fixture never reached the window server");
    /// let active = overlay.diagnostics().expect("active diagnostics");
    /// assert_eq!(active.sharing_type, NSWindowSharingType::None);
    /// assert!(!active.ignores_mouse_events);
    /// assert!(active.accepts_mouse_moved_events);
    ///
    /// overlay
    ///     .apply(&OverlayBehavior::hidden_surface())
    ///     .expect("release input");
    /// overlay.set_visible(false).expect("terminal order out");
    /// flush_window_server();
    /// let idle = overlay.diagnostics().expect("idle diagnostics");
    /// assert!(!idle.is_visible);
    /// assert!(!idle.is_key);
    /// assert!(idle.ignores_mouse_events);
    /// assert!(!is_on_screen(), "a terminal overlay remained in CGWindowList");
    /// overlay
    ///     .restore_native_class()
    ///     .expect("return the window to its owner before close");
    /// window.close();
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if called off the main thread.
    pub fn set_visible(&mut self, visible: bool) -> Result<()> {
        let mtm = main_thread("changing overlay visibility")?;
        // Disable any utility-window fade. During an animation `isVisible` can
        // already be false while CoreGraphics still enumerates the old surface.
        self.window
            .setAnimationBehavior(NSWindowAnimationBehavior::None);
        if visible {
            self.window.orderFrontRegardless();
        } else {
            self.window.setIgnoresMouseEvents(true);
            self.window.setAcceptsMouseMovedEvents(false);
            if !self.window.areCursorRectsEnabled() {
                self.window.enableCursorRects();
            }
            NSCursor::arrowCursor().set();
            self.window.orderOut(None);
            if let Some(lease) = self.presentation_lease.take() {
                lease.release(mtm);
            }
        }
        Ok(())
    }

    /// Sets just the stacking level.
    ///
    /// Separate from [`Self::apply`] because the selection overlay is raised to
    /// [`OverlayLevel::ScreenSaver`] only while it is on screen, and dropped
    /// back afterwards.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if called off the main thread.
    pub fn set_level(&mut self, level: OverlayLevel) -> Result<()> {
        let _mtm = main_thread("setting an overlay window level")?;
        self.window.setLevel(level_value(level));
        Ok(())
    }

    /// Sets the native cursor and makes it stick for as long as this window is
    /// under the pointer.
    ///
    /// # Why [`set_overlay_cursor`] alone is not enough
    ///
    /// That free function calls `-[NSCursor set]` directly, which changes the
    /// system's *current* cursor immediately — but it does not touch this
    /// window's own idea of what the cursor should be. Winit registers a
    /// **cursor rect** covering the whole content view (`-resetCursorRects`,
    /// `-addCursorRect:cursor:`), and AppKit re-applies *that* cursor — via its
    /// own call to `-[NSCursor set]` — the moment the pointer so much as twitches
    /// inside the window, key or not. Winit's rect still says "arrow" (or
    /// whatever egui last asked for) until winit itself observes a pointer
    /// event and updates it, which is exactly the event a retained,
    /// currently-click-through, or freshly expanded window may not have
    /// received yet. That race is why the crosshair so often loses to a plain
    /// arrow on the very first use: the direct `set()` wins for a frame, then
    /// the next native mouse-moved event hands the cursor straight back to
    /// winit's stale rect.
    ///
    /// [`NSWindow::disableCursorRects`] stops that reassertion cold — while
    /// disabled, nothing but an explicit `-[NSCursor set]` call changes the
    /// cursor, which is exactly the ownership the selection overlay needs for
    /// its full lifetime. [`NSWindow::enableCursorRects`] hands the window back
    /// to winit's normal per-frame cursor handling, which is what capture-card
    /// hover relies on. Both are queried before being flipped
    /// ([`NSWindow::areCursorRectsEnabled`]): AppKit documents the pair as
    /// nestable/balanced, like `NSCursor`'s own `hide`/`unhide`, and calling
    /// either one when the window already agrees is a no-op that keeps the
    /// count exactly where a single matching call would have left it.
    ///
    /// # Verifying this on a real machine
    ///
    /// A doctest rather than a `#[test]`, for the reason given on
    /// [`make_nonactivating_panel`]: only the main thread may touch AppKit, and
    /// a doctest is compiled into its own `fn main`.
    ///
    /// ```
    /// use objc2::rc::Retained;
    /// use objc2::{MainThreadMarker, MainThreadOnly};
    /// use objc2_app_kit::{
    ///     NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow,
    ///     NSWindowStyleMask,
    /// };
    /// use objc2_foundation::{NSPoint, NSRect, NSSize};
    /// use scrozz_shell::macos::overlay::MacOverlay;
    /// use scrozz_shell::overlay::OverlayCursor;
    ///
    /// let mtm = MainThreadMarker::new().expect("doctests run on the main thread");
    /// NSApplication::sharedApplication(mtm)
    ///     .setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    ///
    /// let window = unsafe {
    ///     NSWindow::initWithContentRect_styleMask_backing_defer(
    ///         NSWindow::alloc(mtm),
    ///         NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(10.0, 10.0)),
    ///         NSWindowStyleMask::Borderless,
    ///         NSBackingStoreType::Buffered,
    ///         true,
    ///     )
    /// };
    /// unsafe { window.setReleasedWhenClosed(false) };
    /// assert!(window.areCursorRectsEnabled(), "a fresh window starts with rects enabled");
    ///
    /// // SAFETY: the pointer names the live window `window` keeps retained for
    /// // the rest of this block.
    /// let mut overlay = unsafe {
    ///     MacOverlay::from_ns_window(Retained::as_ptr(&window).cast_mut().cast())
    /// }
    /// .expect("adopting a live NSWindow never fails");
    ///
    /// window.setAcceptsMouseMovedEvents(false);
    /// overlay
    ///     .set_cursor(OverlayCursor::Crosshair)
    ///     .expect("the doctest runs on the main thread");
    /// assert!(
    ///     window.acceptsMouseMovedEvents(),
    ///     "pinning the crosshair must also take ownership of pointer motion"
    /// );
    /// assert!(
    ///     !window.areCursorRectsEnabled(),
    ///     "the crosshair must pin the window so winit's own cursor rect cannot revert it"
    /// );
    ///
    /// // A caller that reasserts the same cursor every frame — which is
    /// // exactly what `BehaviorController` does while a selection is live —
    /// // must never leave the window more disabled than one call would have.
    /// // `areCursorRectsEnabled` is queried before every toggle specifically
    /// // to guarantee that.
    /// overlay
    ///     .set_cursor(OverlayCursor::Crosshair)
    ///     .expect("the doctest runs on the main thread");
    /// overlay
    ///     .set_cursor(OverlayCursor::Arrow)
    ///     .expect("the doctest runs on the main thread");
    /// assert!(
    ///     window.areCursorRectsEnabled(),
    ///     "returning to Arrow must hand the window back to winit's hover-driven cursor handling"
    /// );
    ///
    /// overlay
    ///     .restore_native_class()
    ///     .expect("return the window to its owner before close");
    /// window.close();
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if called off the main thread.
    pub fn set_cursor(&self, cursor: OverlayCursor) -> Result<()> {
        let _mtm = main_thread("setting the overlay window cursor")?;
        match cursor {
            OverlayCursor::Crosshair => {
                // A non-activating panel does not reliably receive mouseMoved
                // until opted in. Without those events, the application under
                // the transparent selector keeps restoring its own arrow.
                self.window.setAcceptsMouseMovedEvents(true);
                // Disabling first and setting second means the window can
                // never observe a rect-driven reset in between: no code here
                // yields to the run loop before the crosshair is current.
                if self.window.areCursorRectsEnabled() {
                    self.window.disableCursorRects();
                }
                NSCursor::crosshairCursor().set();
            }
            OverlayCursor::Arrow => {
                NSCursor::arrowCursor().set();
                // Hand the window back to winit's own cursor-rect machinery
                // now that nothing native needs to stay pinned. A card's hover
                // cursor is driven by egui, through winit, and both stop being
                // reachable once this window is left disabled.
                if !self.window.areCursorRectsEnabled() {
                    self.window.enableCursorRects();
                }
            }
        }
        Ok(())
    }

    /// Everything worth asserting about the window's current native state.
    ///
    /// Exists so the panel conversion can be *proved* rather than assumed —
    /// see `tests/overlay.rs`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if called off the main thread.
    pub fn diagnostics(&self) -> Result<OverlayDiagnostics> {
        let mtm = main_thread("inspecting an overlay window")?;
        // SAFETY: reinterpreting an Objective-C object pointer as `AnyObject`.
        let object: &AnyObject = unsafe { &*std::ptr::from_ref(&*self.window).cast::<AnyObject>() };
        let frame = self.window.frame();
        let frame = appkit_to_logical(
            AppKitRect::new(
                frame.origin.x,
                frame.origin.y,
                frame.size.width,
                frame.size.height,
            ),
            display::reference_height(mtm),
        );
        Ok(OverlayDiagnostics {
            class_name: object.class().name().to_string_lossy().into_owned(),
            is_panel: self.window.isKindOfClass(NSPanel::class()),
            has_nonactivating_mask: self
                .window
                .styleMask()
                .contains(NSWindowStyleMask::NonactivatingPanel),
            can_become_key: self.window.canBecomeKeyWindow(),
            can_become_main: self.window.canBecomeMainWindow(),
            level: self.window.level(),
            collection_behavior: self.window.collectionBehavior().0,
            accepts_mouse_moved_events: self.window.acceptsMouseMovedEvents(),
            ignores_mouse_events: self.window.ignoresMouseEvents(),
            sharing_type: self.window.sharingType(),
            window_number: self.window.windowNumber(),
            frame,
            backing_scale: self.window.backingScaleFactor(),
            is_key: self.window.isKeyWindow(),
            is_visible: self.window.isVisible(),
        })
    }
}

impl Drop for MacOverlay {
    fn drop(&mut self) {
        if let Some(lease) = self.presentation_lease.take()
            && let Some(mtm) = objc2::MainThreadMarker::new()
        {
            lease.release(mtm);
        }
    }
}

/// A snapshot of an overlay window's native state.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayDiagnostics {
    /// The window's current Objective-C class name.
    pub class_name: String,
    /// Whether the window now answers as an `NSPanel`.
    pub is_panel: bool,
    /// Whether `NSWindowStyleMaskNonactivatingPanel` is set.
    pub has_nonactivating_mask: bool,
    /// Whether the window can take keyboard focus — needed for Escape.
    pub can_become_key: bool,
    /// Whether the window can become main — must be `false`, or clicking it
    /// activates Scrozz and pulls focus out of the user's work.
    pub can_become_main: bool,
    /// The window's `NSWindowLevel`.
    pub level: NSWindowLevel,
    /// The raw `NSWindowCollectionBehavior` bits.
    pub collection_behavior: usize,
    /// Whether pointer motion is delivered while the panel is not active.
    pub accepts_mouse_moved_events: bool,
    /// Whether pointer input passes through the whole native window.
    pub ignores_mouse_events: bool,
    /// Whether other processes may capture this utility surface.
    pub sharing_type: NSWindowSharingType,
    /// CoreGraphics/AppKit identity used by native enumeration probes.
    pub window_number: isize,
    /// Current native frame in Scrozz's top-left logical coordinates.
    pub frame: LogicalRect,
    /// Native backing pixels per logical point.
    pub backing_scale: f64,
    /// Whether the surface currently owns keyboard focus.
    pub is_key: bool,
    /// Whether the window is currently on screen.
    pub is_visible: bool,
}

impl OverlayWindow for MacOverlay {
    fn set_frame(&mut self, frame: LogicalRect) -> Result<()> {
        let mtm = main_thread("positioning an overlay window")?;
        // Re-read the reference height every time rather than caching it: the
        // user can unplug a display or change the primary between two frames,
        // and a stale height silently offsets every overlay afterwards.
        let reference = display::reference_height(mtm);
        let appkit = logical_to_appkit(frame, reference);
        self.window.setFrame_display(
            NSRect::new(
                objc2_foundation::NSPoint::new(appkit.x, appkit.y),
                objc2_foundation::NSSize::new(appkit.width, appkit.height),
            ),
            true,
        );
        Ok(())
    }

    fn set_click_through(&mut self, passthrough: bool) -> Result<()> {
        let _mtm = main_thread("setting overlay click-through")?;
        // All-or-nothing per window, which is why the capture stack toggles it
        // per frame from the pointer position rather than trying to describe a
        // hit region: once `ignoresMouseEvents` is YES the window receives no
        // mouse events at all and cannot notice the pointer returning.
        self.window.setIgnoresMouseEvents(passthrough);
        self.window.setAcceptsMouseMovedEvents(!passthrough);
        Ok(())
    }
}
