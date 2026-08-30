//! Blocking wrappers around ScreenCaptureKit's asynchronous entry points.
//!
//! ScreenCaptureKit is completion-handler based throughout. A still capture is
//! conceptually synchronous, so each function here builds a [`block2::RcBlock`],
//! hands it to the generated binding, parks on a [`Rendezvous`], and returns an
//! owned object or a [`scrozz_core::Error`].
//!
//! Two hazards are inherent to bridging a completion handler to a blocking
//! call, and both are handled in [`Rendezvous`] rather than at each call site:
//! the handler runs on one of ScreenCaptureKit's own queues, so whatever it
//! delivers crosses a thread boundary; and it delivers autoreleased objects,
//! whose pool can drain before the parked thread wakes.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use block2::{DynBlock, RcBlock};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyClass;
use objc2::{AnyThread, Message, sel};
use objc2_core_foundation::CGRect;
use objc2_core_graphics::{CGImage, CGPreflightScreenCaptureAccess};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
};
use scrozz_core::{Error, Result};

use super::error;
use crate::CaptureCancellation;

/// Enumerating what is shareable is a local query; it should be near-instant.
const ENUMERATE_TIMEOUT: Duration = Duration::from_secs(5);

/// A capture has to round-trip through the window server and encode an image,
/// which on a large multi-display setup is not instant.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Checks Screen Recording permission without requesting it.
///
/// The app owns the just-in-time preflight and is the only code allowed to invoke
/// macOS's alarming system prompt. A backend call can race with revocation, so it
/// still checks immediately before reading pixels and reports denial rather than
/// silently returning wallpaper.
pub(crate) fn ensure_permission() -> Result<()> {
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
    shareable_content_with_cancellation(None)
}

pub(crate) fn shareable_content_with_cancellation(
    cancellation: Option<&CaptureCancellation>,
) -> Result<Retained<SCShareableContent>> {
    if let Some(cancellation) = cancellation {
        cancellation.check()?;
    }
    ensure_permission()?;

    blocking(
        "listing shareable content",
        ENUMERATE_TIMEOUT,
        cancellation,
        |handler| {
            // SAFETY: the arguments match the selector's `BOOL, BOOL, block`.
            unsafe {
                SCShareableContent::
                getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                    true, false, handler,
                );
            }
        },
    )
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
pub(crate) fn capture_image_with_cancellation(
    filter: &SCContentFilter,
    config: &SCStreamConfiguration,
    cancellation: Option<&CaptureCancellation>,
) -> Result<Retained<CGImage>> {
    if let Some(cancellation) = cancellation {
        cancellation.check()?;
    }
    let manager = screenshot_manager()?;
    let selector = sel!(captureImageWithFilter:configuration:completionHandler:);
    if !manager.metaclass().responds_to(selector) {
        return Err(error::unsupported(
            "still capture",
            "this macOS is too old for +[SCScreenshotManager captureImageWithFilter:…]; \
             macOS 14 or later is required",
        ));
    }

    blocking(
        "taking a screenshot",
        CAPTURE_TIMEOUT,
        cancellation,
        |handler| {
            // SAFETY: the arguments match the selector's
            // `SCContentFilter *, SCStreamConfiguration *, block`.
            unsafe {
                SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                    filter,
                    config,
                    Some(handler),
                );
            }
        },
    )
}

/// Starts one screenshot without moving its filter off the calling thread.
///
/// Apple's picker does not document `SCContentFilter` as thread-safe. Its
/// observer therefore calls this immediately with the borrowed callback filter;
/// only the resulting `CGImage` crosses into Scrozz's worker.
pub(crate) fn capture_image_async(
    filter: &SCContentFilter,
    config: &SCStreamConfiguration,
    completion: impl FnOnce(Result<Retained<CGImage>>) + Send + 'static,
) -> Result<()> {
    let manager = screenshot_manager()?;
    let selector = sel!(captureImageWithFilter:configuration:completionHandler:);
    if !manager.metaclass().responds_to(selector) {
        return Err(error::unsupported(
            "still capture",
            "this macOS is too old for +[SCScreenshotManager captureImageWithFilter:…]; \
             macOS 14 or later is required",
        ));
    }

    let completion = Arc::new(Mutex::new(Some(completion)));
    let handler = {
        let completion = Arc::clone(&completion);
        RcBlock::new(move |value: *mut CGImage, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes null or live callback objects. The
            // retained CGImage is Send + Sync in objc2-core-graphics.
            let delivery = unsafe { Delivery::adopt(value, error) };
            let finish = completion
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(finish) = finish {
                finish(delivery.into_result("taking an Apple picker screenshot"));
            }
        })
    };

    // SAFETY: all arguments match the generated selector. The system picker owns
    // its current filter, and ScreenCaptureKit copies the completion block for
    // the asynchronous operation; Scrozz neither retains nor moves the filter.
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            filter,
            config,
            Some(&handler),
        );
    }
    Ok(())
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

/// Whether this OS exposes the still-capture manager used by every Scrozz
/// screenshot path.
pub(crate) fn supports_still_capture() -> bool {
    screenshot_manager().is_ok()
}

/// Captures a rectangle given in global display points.
///
/// The rectangle is display-agnostic: ScreenCaptureKit resolves which displays
/// it covers and returns one image. That is exactly what a region selection
/// dragged across two monitors needs, and what a whole-desktop capture needs.
pub(crate) fn capture_image_in_rect_with_cancellation(
    rect: CGRect,
    cancellation: Option<&CaptureCancellation>,
) -> Result<Retained<CGImage>> {
    if let Some(cancellation) = cancellation {
        cancellation.check()?;
    }
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

    blocking(
        "capturing a rectangle",
        CAPTURE_TIMEOUT,
        cancellation,
        |handler| {
            // SAFETY: the arguments match the selector's `CGRect, block`.
            unsafe {
                SCScreenshotManager::captureImageInRect_completionHandler(rect, Some(handler));
            }
        },
    )
}

/// Allocates an `SCContentFilter` for one of the `initWith…` families.
pub(crate) fn alloc_filter() -> Allocated<SCContentFilter> {
    SCContentFilter::alloc()
}

/// Looks the class up through the runtime rather than [`objc2::ClassType`].
///
/// `SCScreenshotManager::class()` panics when the class is absent, which is
/// precisely the case this needs to report as [`Error::Unsupported`].
fn screenshot_manager() -> Result<&'static AnyClass> {
    AnyClass::get(c"SCScreenshotManager").ok_or_else(|| {
        error::unsupported(
            "still capture",
            "SCScreenshotManager is missing; macOS 14 or later is required",
        )
    })
}

/// Dispatches a completion-handler call and blocks until it answers.
///
/// `dispatch` is handed the block and must pass it to exactly one
/// ScreenCaptureKit entry point. The block is an [`RcBlock`], so ScreenCaptureKit
/// copying it onto its own queue is a reference-count bump rather than a
/// lifetime question, and the closure — along with its strong reference to the
/// rendezvous — lives exactly as long as the block does.
fn blocking<T: Message + 'static>(
    context: &'static str,
    timeout: Duration,
    cancellation: Option<&CaptureCancellation>,
    dispatch: impl FnOnce(&DynBlock<dyn Fn(*mut T, *mut NSError)>),
) -> Result<Retained<T>> {
    let slot = Arc::new(Rendezvous::<T>::new());

    let handler = {
        let slot = Arc::clone(&slot);
        RcBlock::new(move |value: *mut T, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes either null or a live, valid
            // object of the type the selector declares.
            let delivery = unsafe { Delivery::adopt(value, error) };
            slot.fulfil(delivery);
        })
    };

    dispatch(&handler);

    slot.wait_with_cancellation(timeout, cancellation)?
        .ok_or_else(|| timed_out(context))?
        .into_result(context)
}

fn timed_out(context: &str) -> Error {
    Error::Platform(format!(
        "{context}: ScreenCaptureKit did not answer in time; \
         the screen capture service may be wedged"
    ))
}

/// What a completion handler hands back: a result, an error, or neither.
struct Delivery<T: ?Sized> {
    value: Option<Retained<T>>,
    error: Option<Retained<NSError>>,
}

// SAFETY: a delivery crosses exactly one thread boundary, by move, under a
// mutex, and the sending side never touches it again. The delivered types are
// `SCShareableContent`, `CGImage` and `NSError`: immutable, non-thread-affine
// value objects that Apple documents as usable from any thread. `objc2` cannot
// mark them `Send` because it cannot know that in general; here it is known.
unsafe impl<T: ?Sized> Send for Delivery<T> {}

impl<T: Message> Delivery<T> {
    /// Takes ownership of the autoreleased objects a handler was given.
    ///
    /// The retain is the point: these arrive `+0` in ScreenCaptureKit's own
    /// autorelease pool, which can drain the moment the handler returns —
    /// before the parked thread has looked at them.
    ///
    /// # Safety
    ///
    /// Each pointer must be null or a valid, live object of its type.
    unsafe fn adopt(value: *mut T, error: *mut NSError) -> Self {
        Self {
            // SAFETY: upheld by the caller.
            value: unsafe { Retained::retain(value) },
            // SAFETY: upheld by the caller.
            error: unsafe { Retained::retain(error) },
        }
    }
}

impl<T: ?Sized> Delivery<T> {
    /// Prefers the result over the error, since a handler can deliver both.
    fn into_result(self, context: &str) -> Result<Retained<T>> {
        self.value
            .ok_or_else(|| error::from_optional_ns_error(self.error, context))
    }
}

/// A one-shot handover between a completion handler and the thread waiting on it.
///
/// An unclaimed delivery — one that arrived after the wait timed out — is
/// released when the last reference to the rendezvous drops. That matters: a
/// late `CGImage` for a 6K display is tens of megabytes.
struct Rendezvous<T: ?Sized> {
    delivery: Mutex<Option<Delivery<T>>>,
    ready: Condvar,
}

impl<T: ?Sized> Rendezvous<T> {
    fn new() -> Self {
        Self {
            delivery: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    fn fulfil(&self, delivery: Delivery<T>) {
        let mut slot = self.lock();
        *slot = Some(delivery);
        drop(slot);
        self.ready.notify_all();
    }

    fn wait_with_cancellation(
        &self,
        timeout: Duration,
        cancellation: Option<&CaptureCancellation>,
    ) -> Result<Option<Delivery<T>>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Error::Platform("the ScreenCaptureKit deadline overflowed".into()))?;
        let mut slot = self.lock();
        loop {
            if let Some(cancellation) = cancellation {
                cancellation.check()?;
            }
            if slot.is_some() {
                return Ok(slot.take());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let wait = if cancellation.is_some() {
                remaining.min(CANCELLATION_POLL_INTERVAL)
            } else {
                remaining
            };
            (slot, _) = self
                .ready
                .wait_timeout_while(slot, wait, |slot| slot.is_none())
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    #[cfg(test)]
    fn wait(&self, timeout: Duration) -> Option<Delivery<T>> {
        self.wait_with_cancellation(timeout, None)
            .expect("an uncancelled rendezvous wait cannot fail")
    }

    /// A poisoned mutex here means a completion handler panicked. What it
    /// guards is an `Option` either way, so recovering beats bringing down a
    /// capture that may still succeed.
    fn lock(&self) -> MutexGuard<'_, Option<Delivery<T>>> {
        self.delivery.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use objc2::rc::Weak;
    use objc2_foundation::NSObject;

    use super::*;

    #[test]
    fn a_rendezvous_hands_the_delivery_to_the_waiter() {
        let slot = Arc::new(Rendezvous::<NSObject>::new());
        let writer = Arc::clone(&slot);

        thread::spawn(move || {
            writer.fulfil(Delivery {
                value: Some(NSObject::new()),
                error: None,
            });
        });

        let delivery = slot
            .wait(Duration::from_secs(5))
            .expect("the writer fulfils well within the timeout");
        assert!(delivery.value.is_some());
    }

    #[test]
    fn a_rendezvous_nobody_fulfils_times_out_rather_than_hanging() {
        let slot = Rendezvous::<NSObject>::new();
        assert!(slot.wait(Duration::from_millis(50)).is_none());
    }

    #[test]
    fn a_cancelled_rendezvous_returns_without_waiting_for_the_handler() {
        let slot = Rendezvous::<NSObject>::new();
        let cancellation = CaptureCancellation::new();
        cancellation.cancel();

        let error = match slot.wait_with_cancellation(Duration::from_secs(5), Some(&cancellation)) {
            Err(error) => error,
            Ok(_) => panic!("the cancelled wait must stop"),
        };
        assert!(error.is_cancellation());
    }

    #[test]
    fn a_delivery_with_neither_value_nor_error_is_still_an_error() {
        let delivery = Delivery::<NSObject> {
            value: None,
            error: None,
        };
        assert!(delivery.into_result("a test").is_err());
    }

    /// The hazard the old hand-rolled block had to handle by hand: a handler
    /// answering after the waiter gave up must not leak the image it carries.
    #[test]
    fn a_late_delivery_is_released_with_the_rendezvous() {
        let slot = Arc::new(Rendezvous::<NSObject>::new());
        assert!(slot.wait(Duration::from_millis(10)).is_none());

        let object = NSObject::new();
        let weak = Weak::from_retained(&object);
        slot.fulfil(Delivery {
            value: Some(object),
            error: None,
        });

        drop(slot);
        assert!(weak.load().is_none(), "the late delivery was leaked");
    }
}
