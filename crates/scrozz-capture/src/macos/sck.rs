//! Blocking wrappers around ScreenCaptureKit's asynchronous entry points.
//!
//! ScreenCaptureKit is completion-handler based throughout. A still capture is
//! conceptually synchronous, so each function here dispatches the call, parks on
//! a [`Slot`], and returns an owned object or a [`scrozz_core::Error`].
//!
//! The calls go through `msg_send!` rather than the generated bindings because
//! those bindings mention `block2::DynBlock` in their signatures, and `block2`
//! is not a dependency of this crate — see [`super::block`].

use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::Duration;

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{AnyThread, ClassType, msg_send, sel};
use objc2_core_graphics::{
    CGImage, CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess,
};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{SCContentFilter, SCShareableContent, SCStreamConfiguration};
use scrozz_core::{Error, Result};

use super::block::{CompletionBlock, Outcome, Slot};
use super::error;

/// Enumerating what is shareable is a local query; it should be near-instant.
const ENUMERATE_TIMEOUT: Duration = Duration::from_secs(5);

/// A capture has to round-trip through the window server and encode an image,
/// which on a large multi-display setup is not instant.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

/// Checks — and, on first run, requests — Screen Recording permission.
///
/// Per decision D15 the absence of this permission is the expected state on a
/// fresh install, so this asks for it rather than treating it as a fault. The
/// request itself is non-blocking: macOS shows its own prompt and returns
/// immediately, having granted nothing yet. The grant only takes effect for a
/// *future* launch, which is why the remedy string says to relaunch.
pub(crate) fn ensure_permission() -> Result<()> {
    // SAFETY: both are plain C functions with no preconditions.
    if CGPreflightScreenCaptureAccess() {
        return Ok(());
    }

    // SAFETY: as above. The return value is deliberately ignored — a `true`
    // here does not mean this process can capture yet.
    let _ = CGRequestScreenCaptureAccess();

    // SAFETY: as above.
    if CGPreflightScreenCaptureAccess() {
        return Ok(());
    }

    Err(error::permission_denied())
}

/// Fetches the current shareable content: displays, windows and applications.
///
/// Desktop windows — the wallpaper, the Dock's backing window and similar — are
/// excluded, because they are not things a user would ever pick from a window
/// list. Off-screen windows are kept, so minimised and hidden windows can still
/// be listed and reported as not visible.
pub(crate) fn shareable_content() -> Result<Retained<SCShareableContent>> {
    ensure_permission()?;

    let slot = Slot::new();
    let mut block = CompletionBlock::new(&slot);
    let handler = block.as_arg();

    // SAFETY: the selector is
    // `+[SCShareableContent getShareableContentExcludingDesktopWindows:onScreenWindowsOnly:completionHandler:]`,
    // whose arguments are `BOOL`, `BOOL` and a block. `handler` declares the
    // block encoding, and the block outlives the call because it owns a strong
    // reference to `slot` and is copied by ScreenCaptureKit.
    unsafe {
        let _: () = msg_send![
            SCShareableContent::class(),
            getShareableContentExcludingDesktopWindows: true,
            onScreenWindowsOnly: false,
            completionHandler: handler,
        ];
    }

    let outcome = slot
        .wait(ENUMERATE_TIMEOUT)
        .ok_or_else(|| timed_out("listing shareable content"))?;

    // SAFETY: on success `value` is a `+1` `SCShareableContent`.
    unsafe { claim(outcome, "listing shareable content") }
}

/// Takes a single screenshot through the given filter.
///
/// Uses `SCScreenshotManager`, which is macOS 14 and later. On macOS 12.3–13 the
/// equivalent is markedly more machinery for the same result: build an
/// `SCStream` with the same filter and configuration, attach an
/// `SCStreamOutput` delegate on a private dispatch queue, start the stream, keep
/// the first `CMSampleBuffer` whose `SCStreamFrameInfoStatus` attachment is
/// `SCFrameStatusComplete`, pull its `CVPixelBuffer`, then stop and tear down
/// the stream. Scrozz's floor is macOS 14, so that path is described rather
/// than implemented; the class check below turns an older system into a clear
/// [`Error::Unsupported`] instead of a crash.
pub(crate) fn capture_image(
    filter: &SCContentFilter,
    config: &SCStreamConfiguration,
) -> Result<Retained<CGImage>> {
    let manager = screenshot_manager()?;
    let selector = sel!(captureImageWithFilter:configuration:completionHandler:);
    if !manager.metaclass().responds_to(selector) {
        return Err(error::unsupported(
            "still capture",
            "this macOS is too old for +[SCScreenshotManager captureImageWithFilter:…]; \
             macOS 14 or later is required",
        ));
    }

    let slot = Slot::new();
    let mut block = CompletionBlock::new(&slot);
    let handler = block.as_arg();

    // SAFETY: the selector's arguments are `SCContentFilter *`,
    // `SCStreamConfiguration *` and a block, which is what is passed.
    unsafe {
        let _: () = msg_send![
            manager,
            captureImageWithFilter: filter,
            configuration: config,
            completionHandler: handler,
        ];
    }

    let outcome = slot
        .wait(CAPTURE_TIMEOUT)
        .ok_or_else(|| timed_out("taking a screenshot"))?;

    // SAFETY: on success `value` is a `+1` `CGImage`. ScreenCaptureKit vends it
    // as a CoreFoundation object, which `Retained` manages identically to an
    // Objective-C one — `CFRelease` and `objc_release` are the same call.
    unsafe { claim(outcome, "taking a screenshot") }
}

/// Whether `+[SCScreenshotManager captureImageInRect:completionHandler:]` exists.
///
/// It arrived after the rest of `SCScreenshotManager`, and it is the only API
/// that captures an arbitrary rectangle in global display space — spanning
/// displays if need be — without compositing anything by hand.
pub(crate) fn supports_capture_in_rect() -> bool {
    screenshot_manager().is_ok_and(|manager| {
        manager
            .metaclass()
            .responds_to(sel!(captureImageInRect:completionHandler:))
    })
}

/// Captures a rectangle given in global display points.
///
/// The rectangle is display-agnostic: ScreenCaptureKit resolves which displays
/// it covers and returns one image. That is exactly what a region selection
/// dragged across two monitors needs, and what a whole-desktop capture needs.
pub(crate) fn capture_image_in_rect(rect: objc2_core_foundation::CGRect) -> Result<Retained<CGImage>> {
    ensure_permission()?;

    let manager = screenshot_manager()?;
    if !manager
        .metaclass()
        .responds_to(sel!(captureImageInRect:completionHandler:))
    {
        return Err(error::unsupported(
            "capturing a rectangle spanning displays",
            "this macOS lacks +[SCScreenshotManager captureImageInRect:completionHandler:]",
        ));
    }

    let slot = Slot::new();
    let mut block = CompletionBlock::new(&slot);
    let handler = block.as_arg();

    // SAFETY: the selector takes a `CGRect` by value and a block.
    unsafe {
        let _: () = msg_send![
            manager,
            captureImageInRect: rect,
            completionHandler: handler,
        ];
    }

    let outcome = slot
        .wait(CAPTURE_TIMEOUT)
        .ok_or_else(|| timed_out("capturing a rectangle"))?;

    // SAFETY: on success `value` is a `+1` `CGImage`.
    unsafe { claim(outcome, "capturing a rectangle") }
}

/// Allocates an `SCContentFilter` for one of the `initWith…` families.
pub(crate) fn alloc_filter() -> Allocated<SCContentFilter> {
    SCContentFilter::alloc()
}

fn screenshot_manager() -> Result<&'static AnyClass> {
    AnyClass::get(c"SCScreenshotManager").ok_or_else(|| {
        error::unsupported(
            "still capture",
            "SCScreenshotManager is missing; macOS 14 or later is required",
        )
    })
}

fn timed_out(context: &str) -> Error {
    Error::Platform(format!(
        "{context}: ScreenCaptureKit did not answer in time; \
         the screen capture service may be wedged"
    ))
}

/// Converts a completion handler's outcome into an owned object or an error.
///
/// # Safety
///
/// `outcome.value`, when non-null, must be a `+1` reference to an object of
/// type `T`, and `outcome.error`, when non-null, a `+1` `NSError`.
unsafe fn claim<T: objc2::Message>(outcome: Outcome, context: &str) -> Result<Retained<T>> {
    // Take ownership of the error first, so it is released either way.
    let error = NonNull::new(outcome.error.cast::<NSError>())
        // SAFETY: a `+1` `NSError`, per this function's contract.
        .and_then(|error| unsafe { Retained::from_raw(error.as_ptr()) });

    match NonNull::new(outcome.value.cast::<T>()) {
        // SAFETY: a `+1` `T`, per this function's contract.
        Some(value) => unsafe { Retained::from_raw(value.as_ptr()) }
            .ok_or_else(|| Error::Platform(format!("{context}: unexpected null result"))),
        None => Err(error::from_optional_ns_error(error, context)),
    }
}

/// Reinterprets an object pointer, for the few places where ScreenCaptureKit's
/// static types and the runtime's disagree.
#[expect(dead_code, reason = "kept alongside `claim` for symmetry of the bridge")]
pub(crate) fn as_object<T>(value: &T) -> *mut AnyObject {
    std::ptr::from_ref(value).cast::<AnyObject>().cast_mut()
}

/// Marker for the pointer width assumption baked into the block ABI shim.
const _: () = assert!(size_of::<*mut c_void>() == 8);
