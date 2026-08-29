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
    NSFloatingWindowLevel, NSNormalWindowLevel, NSPanel, NSPopUpMenuWindowLevel,
    NSStatusWindowLevel, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowLevel,
    NSWindowStyleMask,
};
use objc2_core_graphics::CGShieldingWindowLevel;
use objc2_foundation::NSRect;
use scrozz_core::{Error, LogicalRect, Result};

use crate::OverlayWindow;
use crate::macos::{display, main_thread};
use crate::overlay::{OverlayBehavior, OverlayLevel, OverlayReport, logical_to_appkit};

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
/// [`OverlayLevel::Shielding`] is read from `CGShieldingWindowLevel()` at call
/// time because Apple documents it as "the level above which the system does
/// not draw" rather than as a fixed number.
#[must_use]
pub fn level_value(level: OverlayLevel) -> NSWindowLevel {
    match level {
        OverlayLevel::Normal => NSNormalWindowLevel,
        OverlayLevel::Floating => NSFloatingWindowLevel,
        OverlayLevel::Status => NSStatusWindowLevel,
        OverlayLevel::AboveMenuBar => NSPopUpMenuWindowLevel,
        // `CGShieldingWindowLevel` is a pure query with no arguments and no
        // failure mode; the binding is safe in objc2-core-graphics 0.3.
        OverlayLevel::Shielding => CGShieldingWindowLevel() as NSWindowLevel,
    }
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
}

impl MacOverlay {
    /// Returns whether AppKit currently ignores mouse events for this window.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] off the main thread.
    pub fn click_through(&self) -> Result<bool> {
        let _mtm = main_thread("reading overlay click-through")?;
        Ok(self.window.ignoresMouseEvents())
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
            non_activating: false,
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
        let _mtm = main_thread("configuring an overlay window")?;

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
        window.setOpaque(behavior.opaque);
        window.setHasShadow(behavior.has_shadow);
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

    /// Sets just the stacking level.
    ///
    /// Separate from [`Self::apply`] because the selection overlay is raised to
    /// [`OverlayLevel::Shielding`] only while it is on screen, and dropped back
    /// afterwards.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if called off the main thread.
    pub fn set_level(&mut self, level: OverlayLevel) -> Result<()> {
        let _mtm = main_thread("setting an overlay window level")?;
        self.window.setLevel(level_value(level));
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
            is_visible: self.window.isVisible(),
        })
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
        Ok(())
    }
}
