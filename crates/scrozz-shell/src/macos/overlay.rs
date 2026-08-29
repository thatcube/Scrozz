//! Retrofitting a live `NSWindow` into a non-activating floating panel.
//!
//! # The problem this solves
//!
//! A plain `NSWindow` **activates its application when clicked**. Scrozz's
//! capture cards are clickable — they are dragged into other apps, annotated,
//! dismissed — and if clicking one pulled focus, the user's cursor would jump
//! out of the sentence they were typing. Per decision D27 that is the whole
//! difference between a tool that lives beside your work and one that
//! interrupts it.
//!
//! The AppKit construct that does not activate is `NSPanel` with
//! `NSWindowStyleMaskNonactivatingPanel`. But eframe/winit hands us an
//! `NSWindow`, already created, already backed by a Metal layer, already
//! attached to an event loop. So the panel-ness has to be applied *afterwards*.
//!
//! # How, and why it is sound
//!
//! `object_setClass` — the same trick the `tauri-nspanel` plugin uses. The
//! instance keeps its memory and its ivars; only its `isa` pointer changes, so
//! it starts answering `NSPanel`'s methods. This is sound exactly when the new
//! class has **the same instance layout** as the old one, which holds here
//! because `NSPanel` declares no ivars over `NSWindow` and neither does the
//! subclass built below.
//!
//! That is a fact about Apple's implementation, not a guarantee in the header,
//! and winit is free to add ivars to its own `NSWindow` subclass at any
//! release. So it is **checked at runtime** rather than assumed:
//! [`make_nonactivating_panel`] compares `class_getInstanceSize` for both
//! classes and *refuses the swizzle* if they differ, returning an explanation
//! instead of corrupting the window. A capture stack that steals focus is a
//! bad day; a capture stack that scribbles past the end of an object is a
//! crash report nobody can read.
//!
//! # Status as of winit 0.30.13
//!
//! `WinitWindow` is declared with `impl DeclaredClass for WinitWindow {}` and no
//! `type Ivars`, which means no additional storage and therefore the same
//! instance size as `NSWindow`. The conversion is expected to succeed on a
//! winit window, and is verified to succeed on a plain `NSWindow` by the
//! doctest on [`make_nonactivating_panel`]. Note that winit overrides
//! `canBecomeMainWindow` to return YES; the swizzle replaces that with the
//! override below, which is the point.
//!
//! Should a future winit gain ivars, the size check turns that into a clear
//! error at the call site rather than memory corruption — the second doctest on
//! [`make_nonactivating_panel`] demonstrates exactly that path against a
//! deliberately fattened subclass.
//!
//! # What the subclass overrides, and why each one
//!
//! - `canBecomeKeyWindow` → **YES**. Borderless windows return NO by default,
//!   and a panel that cannot become key never receives a keystroke — which
//!   would mean Escape could not cancel the selection overlay. Key status is
//!   *not* what steals focus.
//! - `canBecomeMainWindow` → **NO**. Main-window status is what drags the
//!   application forward. Refusing it is half of "do not steal focus"; the
//!   `NonactivatingPanel` style mask is the other half.
//! - `isFloatingPanel` → **YES**. Keeps the panel above its app's ordinary
//!   windows and out of the standard window ordering.

use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, NSObjectProtocol, Sel};
use objc2::{ClassType, sel};
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
    OverlayBehavior, OverlayCursor, OverlayLevel, OverlayReport, logical_to_appkit,
};

/// Name of the runtime-built `NSPanel` subclass windows are swizzled into.
///
/// A `CStr` because `objc2` 0.6 registers classes by C string, and prefixed
/// because the Objective-C runtime has one flat global namespace shared with
/// every framework in the process.
const PANEL_CLASS_NAME: &std::ffi::CStr = c"ScrozzOverlayPanel";

extern "C" fn can_become_key(_this: &AnyObject, _sel: Sel) -> Bool {
    Bool::YES
}

extern "C" fn can_become_main(_this: &AnyObject, _sel: Sel) -> Bool {
    Bool::NO
}

extern "C" fn is_floating_panel(_this: &AnyObject, _sel: Sel) -> Bool {
    Bool::YES
}

/// Registers (once) and returns the overlay panel class.
///
/// Idempotent: the second call finds the class already registered. All callers
/// are main-thread-only, so there is no registration race to guard against.
fn overlay_panel_class() -> Result<&'static AnyClass> {
    if let Some(existing) = AnyClass::get(PANEL_CLASS_NAME) {
        return Ok(existing);
    }

    let mut builder = ClassBuilder::new(PANEL_CLASS_NAME, NSPanel::class()).ok_or_else(|| {
        Error::Platform(
            "could not allocate the ScrozzOverlayPanel class; a class with that name \
             already exists with a different superclass"
                .to_owned(),
        )
    })?;

    // SAFETY: each function's Rust signature matches the Objective-C method it
    // is registered under — `(id self, SEL _cmd) -> BOOL` — and all three are
    // overrides of existing `NSWindow` methods with exactly that shape, which
    // `add_method` re-checks against the superclass encoding under
    // `debug_assertions`.
    unsafe {
        builder.add_method(
            sel!(canBecomeKeyWindow),
            can_become_key as extern "C" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(canBecomeMainWindow),
            can_become_main as extern "C" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(isFloatingPanel),
            is_floating_panel as extern "C" fn(_, _) -> _,
        );
    }

    Ok(builder.register())
}

/// Converts a live `NSWindow` into a non-activating floating `NSPanel`.
///
/// Returns a description of what was done, for [`OverlayReport::detail`].
///
/// # Errors
///
/// Returns [`Error::Platform`] if the panel class cannot be registered, or if
/// the window's class has a different instance size to the panel subclass — in
/// which case **nothing is modified**. The message names both sizes so the
/// cause is diagnosable from a bug report alone.
///
/// # Safety
///
/// Must be called on the main thread, with a window that is not currently
/// mid-`-close`. Both hold for a window owned by a live winit event loop.
///
/// # Verifying this on a real machine
///
/// The example below is the actual proof that the conversion works, and it is a
/// doctest rather than a `#[test]` for a specific reason: libtest runs every
/// test on a spawned thread — even under `--test-threads=1` — and AppKit
/// windows may only be touched from the main thread. A doctest is compiled into
/// its own `fn main`, so it runs where AppKit needs it to.
///
/// The window is never ordered front and the process is marked
/// [`NSApplicationActivationPolicy::Prohibited`] before it exists, so nothing
/// appears on screen and nothing can steal focus while the suite runs.
///
/// ```
/// use objc2::runtime::NSObjectProtocol;
/// use objc2::{ClassType, MainThreadMarker, MainThreadOnly};
/// use objc2_app_kit::{
///     NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSPanel, NSWindow,
///     NSWindowStyleMask,
/// };
/// use objc2_foundation::{NSPoint, NSRect, NSSize};
/// use scrozz_shell::macos::overlay::make_nonactivating_panel;
///
/// let mtm = MainThreadMarker::new().expect("doctests run on the main thread");
///
/// // Belt: a prohibited app has no Dock icon and cannot become active.
/// NSApplication::sharedApplication(mtm)
///     .setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
///
/// // Braces: `defer: true` means the window server is not asked for a surface
/// // until the window is ordered in, and it never is.
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
///
/// assert!(!window.isKindOfClass(NSPanel::class()), "fixture starts as a plain NSWindow");
///
/// // SAFETY: main thread, live window, not closing.
/// let detail = unsafe { make_nonactivating_panel(&window) }
///     .expect("a plain NSWindow converts into a non-activating panel");
///
/// assert!(window.isKindOfClass(NSPanel::class()), "did not become an NSPanel: {detail}");
/// assert!(
///     window.styleMask().contains(NSWindowStyleMask::NonactivatingPanel),
///     "NSWindowStyleMaskNonactivatingPanel was not applied: {detail}"
/// );
/// // Key delivers keystrokes; main is what makes an app frontmost. The panel
/// // must accept the first and refuse the second.
/// assert!(window.canBecomeKeyWindow(), "a panel that cannot be key cannot read Escape");
/// assert!(!window.canBecomeMainWindow(), "becoming main is the focus theft this prevents");
/// assert!(!window.isVisible(), "the fixture must never reach the screen");
///
/// window.close();
/// ```
///
/// # What happens to a subclass that adds storage
///
/// winit, eframe and tauri all hand back an `NSWindow` *subclass* rather than a
/// bare `NSWindow`, and a subclass that declares its own ivars has a larger
/// instance size. Swizzling such an object into this panel class would leave
/// the runtime believing the object is smaller than it is. That is refused, and
/// the refusal is the whole reason for the size check:
///
/// ```
/// use objc2::rc::Allocated;
/// use objc2::runtime::{AnyClass, ClassBuilder, NSObjectProtocol};
/// use objc2::{ClassType, MainThreadMarker};
/// use objc2_app_kit::{
///     NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSPanel, NSWindow,
///     NSWindowStyleMask,
/// };
/// use objc2_foundation::{NSPoint, NSRect, NSSize};
/// use scrozz_shell::macos::overlay::make_nonactivating_panel;
///
/// let mtm = MainThreadMarker::new().expect("doctests run on the main thread");
/// NSApplication::sharedApplication(mtm)
///     .setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
///
/// // Stand in for a toolkit's own window subclass: one extra ivar, so eight
/// // more bytes per instance than plain NSWindow.
/// let fat = AnyClass::get(c"ScrozzFatWindowProbe").unwrap_or_else(|| {
///     let mut builder = ClassBuilder::new(c"ScrozzFatWindowProbe", NSWindow::class())
///         .expect("the probe class name is unused");
///     builder.add_ivar::<usize>(c"scrozz_probe_storage");
///     builder.register()
/// });
/// assert!(fat.instance_size() > NSWindow::class().instance_size());
///
/// // SAFETY: `+alloc` on a class that descends from NSWindow yields an
/// // uninitialised NSWindow, which the inherited designated initialiser then
/// // initialises. `defer: true` and no `orderFront:` mean it never reaches the
/// // screen.
/// let window = unsafe {
///     let allocated: Allocated<NSWindow> = objc2::msg_send![fat, alloc];
///     NSWindow::initWithContentRect_styleMask_backing_defer(
///         allocated,
///         NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(10.0, 10.0)),
///         NSWindowStyleMask::Borderless,
///         NSBackingStoreType::Buffered,
///         true,
///     )
/// };
/// unsafe { window.setReleasedWhenClosed(false) };
///
/// // SAFETY: main thread, live window, not closing.
/// let outcome = unsafe { make_nonactivating_panel(&window) };
///
/// let error = outcome.expect_err("a larger subclass must not be swizzled");
/// let message = error.to_string();
/// assert!(message.contains("instance layouts differ"), "unhelpful error: {message}");
/// assert!(message.contains("ScrozzFatWindowProbe"), "the error must name the class: {message}");
///
/// // And crucially: nothing was changed. The caller gets a window that still
/// // works, merely one that will activate the app when clicked.
/// assert!(!window.isKindOfClass(NSPanel::class()));
/// assert!(!window.styleMask().contains(NSWindowStyleMask::NonactivatingPanel));
///
/// window.close();
/// ```
pub unsafe fn make_nonactivating_panel(window: &NSWindow) -> Result<String> {
    // SAFETY: every Objective-C object is an `AnyObject`; this is a
    // reinterpretation of the same pointer, not a change of provenance.
    let object: &AnyObject = unsafe { &*std::ptr::from_ref(window).cast::<AnyObject>() };
    let current = object.class();

    let panel_class = overlay_panel_class()?;
    let current_size = current.instance_size();
    let panel_size = panel_class.instance_size();

    if current_size != panel_size {
        return Err(Error::Platform(format!(
            "refusing to isa-swizzle {} ({current_size} bytes) into {} ({panel_size} bytes): \
             the instance layouts differ, so the window would be reinterpreted as an object \
             of the wrong size. The window keeps its original class and will activate the \
             app when clicked.",
            current.name().to_string_lossy(),
            PANEL_CLASS_NAME.to_string_lossy(),
        )));
    }

    // SAFETY: the instance sizes are equal (checked immediately above), the new
    // class descends from `NSWindow` exactly as the old one does, and neither
    // `NSPanel` nor `ScrozzOverlayPanel` declares an ivar, so every byte of the
    // instance keeps its meaning.
    let previous = unsafe { AnyObject::set_class(object, panel_class) };

    // The style-mask bit only has meaning on an `NSPanel`, which is why it is
    // set after the class change and not before.
    window.setStyleMask(window.styleMask() | NSWindowStyleMask::NonactivatingPanel);

    Ok(format!(
        "{} -> {} (instance size {current_size} bytes, unchanged)",
        previous.name().to_string_lossy(),
        PANEL_CLASS_NAME.to_string_lossy(),
    ))
}

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

/// A native macOS overlay: an `NSWindow` someone else created, retrofitted.
///
/// Holds a strong reference to the window, so it is safe to keep across frames,
/// but does **not** own it: dropping a `MacOverlay` neither closes nor releases
/// the underlying window beyond its own retain.
#[derive(Debug)]
pub struct MacOverlay {
    window: Retained<NSWindow>,
    non_activating: bool,
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
            non_activating: false,
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
            non_activating: false,
            presentation_lease: None,
        })
    }

    /// The underlying window, for callers already inside AppKit.
    #[must_use]
    pub fn window(&self) -> &NSWindow {
        &self.window
    }

    /// Whether the window is now a non-activating panel.
    ///
    /// `false` means clicking the overlay will pull focus to Scrozz.
    #[must_use]
    pub const fn is_non_activating(&self) -> bool {
        self.non_activating
    }

    /// Applies a complete overlay behaviour to the window.
    ///
    /// Order matters: the class change happens first, because the
    /// `NonactivatingPanel` style-mask bit is only meaningful on an `NSPanel`,
    /// and `setStyleMask:` on a plain `NSWindow` would silently do nothing
    /// useful.
    ///
    /// A failed panel conversion is **not** an error: everything else is still
    /// applied and the outcome is reported in the returned [`OverlayReport`],
    /// because a floating overlay that takes focus is degraded, not broken.
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

        // SAFETY: we are on the main thread (checked above) and hold a strong
        // reference to a live window.
        let conversion = unsafe { make_nonactivating_panel(&self.window) };
        let detail = match conversion {
            Ok(detail) => {
                self.non_activating = true;
                detail
            }
            Err(error) => {
                self.non_activating = false;
                tracing::warn!(%error, "overlay will activate the app when clicked");
                error.to_string()
            }
        };

        let window = &self.window;
        window.setLevel(level_value(behavior.level));
        window.setCollectionBehavior(collection_behavior(behavior));
        window.setHidesOnDeactivate(behavior.hides_on_deactivate);
        window.setIgnoresMouseEvents(behavior.click_through);
        window.setAcceptsMouseMovedEvents(!behavior.click_through);
        window.setOpaque(behavior.opaque);
        window.setHasShadow(behavior.has_shadow);
        window.setSharingType(if behavior.capture_excluded {
            NSWindowSharingType::None
        } else {
            NSWindowSharingType::ReadOnly
        });
        window.setMovable(behavior.movable);
        window.setMovableByWindowBackground(false);

        // `ScrozzOverlayPanel` answers `canBecomeKeyWindow` with YES
        // unconditionally, because a panel that can never be key can never read
        // Escape. Whether a *click* takes key status is a separate switch, and
        // this is it: `becomesKeyOnlyIfNeeded` defers key status until the user
        // hits a control that actually needs typing.
        if let Some(panel) = window.downcast_ref::<NSPanel>() {
            panel.setBecomesKeyOnlyIfNeeded(!behavior.accepts_key);
            panel.setFloatingPanel(behavior.level >= OverlayLevel::Floating);
        }

        Ok(OverlayReport {
            non_activating: self.non_activating,
            detail,
        })
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
    /// overlay.set_visible(true).expect("order front");
    /// flush_window_server();
    /// assert!(is_on_screen(), "the active fixture never reached the window server");
    /// let active = overlay.diagnostics().expect("active diagnostics");
    /// assert_eq!(active.sharing_type, NSWindowSharingType::None);
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
    /// window.close();
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if called off the main thread.
    pub fn set_visible(&mut self, visible: bool) -> Result<()> {
        let mtm = main_thread("changing overlay visibility")?;
        // NSPanel otherwise infers a utility-window fade. During that animation
        // `isVisible` is already false while CoreGraphics can still enumerate
        // the old fullscreen surface, exactly the external-picker failure this
        // method exists to prevent.
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
    /// let overlay = unsafe {
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
        let _mtm = main_thread("inspecting an overlay window")?;
        // SAFETY: reinterpreting an Objective-C object pointer as `AnyObject`.
        let object: &AnyObject = unsafe { &*std::ptr::from_ref(&*self.window).cast::<AnyObject>() };
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
