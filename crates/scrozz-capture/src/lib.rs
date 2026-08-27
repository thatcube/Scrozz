//! Still capture backends.
//!
//! One implementation of [`scrozz_core::CaptureBackend`] per platform, selected
//! at runtime by [`backend`]. Nothing above this crate knows which one it got.
//!
//! # Platform reality
//!
//! - **macOS** — `ScreenCaptureKit` on macOS 12.3+, which is the only path that
//!   still works well and the only one Apple maintains.
//! - **Windows** — `Windows.Graphics.Capture` (WGC) on Windows 10 1903+, with
//!   `BitBlt`/DXGI as the fallback for older builds and odd GPU configurations.
//! - **Linux/X11** — direct `XGetImage`/XShm. Full window enumeration, absolute
//!   positioning, everything works.
//! - **Linux/Wayland** — the `xdg-desktop-portal` `ScreenCast` interface, and
//!   only that. Per decision D8 the gaps are real and documented rather than
//!   papered over: **there is no window enumeration protocol at all**, so
//!   [`scrozz_core::TargetEnumerator::windows`] returns
//!   [`scrozz_core::Error::Unsupported`]. An OS-id-bearing window request is also
//!   refused because the portal picker does not return an identity that can be
//!   checked against that id.

// Platform APIs are reached through objc2 / windows-rs / x11rb, all of which
// require `unsafe`. It is confined to this crate: every crate above it in the
// dependency graph forbids unsafe outright.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
pub use macos::ScreenCaptureKitBackend;

use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::task::{Context, Poll, Waker};

use scrozz_core::{Capture, CaptureBackend, CaptureRequest, Error, Frame, Result};

/// Reserved [`scrozz_core::WindowId`] for explicit Wayland portal selection.
///
/// A request using this value means "capture exactly one window chosen in the
/// desktop portal's own picker." It does not claim an enumerable OS window id,
/// title, owner, geometry, crop, shadow, corner shape, or alpha association.
/// Every other window id is rejected by the Wayland backend before permission.
pub const WAYLAND_PORTAL_PICKER_WINDOW_ID: &str = "xdg-desktop-portal-picker";

/// Cooperative cancellation for an interactive capture acquisition.
///
/// The token is one-way: after [`cancel`](Self::cancel), every acquisition using
/// it returns [`Error::Cancelled`]. On Wayland, cancellation closes the active
/// ScreenCast session so a portal picker cannot hold application shutdown open.
/// Existing [`CaptureBackend`] and [`FrameSession`] APIs remain unchanged;
/// callers that own a shutdown lifecycle can opt into the cancellable free
/// functions.
#[derive(Clone, Debug, Default)]
pub struct CaptureCancellation {
    inner: Arc<CancellationState>,
}

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    next_waiter: AtomicU64,
    waiters: Mutex<Vec<(u64, Waker)>>,
}

impl CaptureCancellation {
    /// Creates a token in the active state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels this token and wakes every pending portal acquisition.
    pub fn cancel(&self) {
        if self.inner.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }

        let waiters = self
            .inner
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect::<Vec<_>>();
        for (_, waiter) in waiters {
            waiter.wake();
        }
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    pub(crate) fn cancelled(&self) -> Cancelled<'_> {
        Cancelled {
            cancellation: self,
            id: self.inner.next_waiter.fetch_add(1, Ordering::Relaxed),
            registered: false,
        }
    }
}

struct Cancelled<'a> {
    cancellation: &'a CaptureCancellation,
    id: u64,
    registered: bool,
}

impl Future for Cancelled<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.cancellation.is_cancelled() {
            return Poll::Ready(());
        }

        let mut waiters = self
            .cancellation
            .inner
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.cancellation.is_cancelled() {
            return Poll::Ready(());
        }
        match waiters.iter_mut().find(|(id, _)| *id == self.id) {
            Some((_, waiter)) if !waiter.will_wake(context.waker()) => {
                *waiter = context.waker().clone();
            }
            Some(_) => {}
            None => waiters.push((self.id, context.waker().clone())),
        }
        drop(waiters);
        self.registered = true;
        Poll::Pending
    }
}

impl Drop for Cancelled<'_> {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        self.cancellation
            .inner
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(id, _)| *id != self.id);
    }
}

/// Supplies viewport frames from one logical capture session.
///
/// A platform may keep native resources open between calls. On Wayland this is
/// the difference between reusing one portal/PipeWire stream and reopening the
/// permission flow for every scrolling viewport. Dropping the session releases
/// every native resource it owns.
pub trait FrameSession {
    /// Captures the viewport in its current state.
    fn capture_frame(&mut self) -> Result<Frame>;

    /// Human-readable source name for diagnostics.
    fn name(&self) -> &str;
}

/// Repeated-frame fallback for platforms whose ordinary backend is already
/// efficient enough to reopen per frame.
struct BackendFrameSession {
    backend: Box<dyn CaptureBackend>,
    request: CaptureRequest,
}

impl FrameSession for BackendFrameSession {
    fn capture_frame(&mut self) -> Result<Frame> {
        Ok(self.backend.capture(&self.request)?.frame)
    }

    fn name(&self) -> &str {
        self.backend.name()
    }
}

pub(crate) fn backend_frame_session(
    backend: Box<dyn CaptureBackend>,
    request: CaptureRequest,
) -> Box<dyn FrameSession> {
    Box::new(BackendFrameSession { backend, request })
}

/// Constructs the best capture backend for the running system.
///
/// # Errors
///
/// Returns [`scrozz_core::Error::Unsupported`] if no backend can serve this
/// platform or compositor.
pub fn backend() -> Result<Box<dyn CaptureBackend>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::ScreenCaptureKitBackend::new()))
    }

    #[cfg(target_os = "linux")]
    {
        linux::backend()
    }

    #[cfg(target_os = "windows")]
    {
        windows::backend()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        todo!("select and construct the platform capture backend")
    }
}

/// Captures once while allowing an owner to cancel interactive acquisition.
///
/// On Wayland this cancellation remains active while waiting for the portal
/// picker and while opening the PipeWire remote. Other backends check the token
/// before entering their ordinary synchronous capture path.
///
/// # Errors
///
/// Returns [`Error::Cancelled`] when `cancellation` has been cancelled, otherwise
/// the same errors as [`backend`] and [`CaptureBackend::capture`].
pub fn capture_with_cancellation(
    request: &CaptureRequest,
    cancellation: &CaptureCancellation,
) -> Result<Capture> {
    #[cfg(target_os = "linux")]
    {
        linux::capture_with_cancellation(request, cancellation)
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        cancellation.check()?;
        backend()?.capture(request)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (request, cancellation);
        todo!("select and construct the platform capture backend")
    }
}

/// Opens a frame source suitable for a scrolling capture.
///
/// Wayland keeps one portal grant and PipeWire stream alive across calls.
/// Other platforms currently adapt their ordinary capture backend.
///
/// # Errors
///
/// Returns the same platform, permission, and target errors as [`backend`] and
/// [`FrameSession::capture_frame`].
pub fn frame_session(request: CaptureRequest) -> Result<Box<dyn FrameSession>> {
    #[cfg(target_os = "linux")]
    {
        linux::frame_session(request)
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        Ok(backend_frame_session(backend()?, request))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = request;
        todo!("select and construct the platform frame session")
    }
}

/// Opens a frame source with cancellable interactive acquisition.
///
/// Once construction succeeds, frame delivery uses the unchanged synchronous
/// [`FrameSession`] API. On Wayland, dropping that session still disconnects
/// PipeWire before closing the portal session.
///
/// # Errors
///
/// Returns [`Error::Cancelled`] when `cancellation` is triggered during portal
/// acquisition, otherwise the same errors as [`frame_session`].
pub fn frame_session_with_cancellation(
    request: CaptureRequest,
    cancellation: &CaptureCancellation,
) -> Result<Box<dyn FrameSession>> {
    #[cfg(target_os = "linux")]
    {
        linux::frame_session_with_cancellation(request, cancellation)
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        cancellation.check()?;
        Ok(backend_frame_session(backend()?, request))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (request, cancellation);
        todo!("select and construct the platform frame session")
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::task::{Context, Poll, Wake, Waker};

    use super::CaptureCancellation;

    struct WakeFlag(AtomicBool);

    impl Wake for WakeFlag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn cancellation_is_one_way_and_wakes_every_waiter() {
        let cancellation = CaptureCancellation::new();
        let mut futures = [
            Box::pin(cancellation.cancelled()),
            Box::pin(cancellation.cancelled()),
        ];
        let flags = [
            Arc::new(WakeFlag(AtomicBool::new(false))),
            Arc::new(WakeFlag(AtomicBool::new(false))),
        ];

        for (future, flag) in futures.iter_mut().zip(&flags) {
            let waker = Waker::from(Arc::clone(flag));
            let mut context = Context::from_waker(&waker);
            assert_eq!(future.as_mut().poll(&mut context), Poll::Pending);
        }

        cancellation.cancel();
        cancellation.cancel();
        for (future, flag) in futures.iter_mut().zip(&flags) {
            assert!(flag.0.load(Ordering::Acquire), "every waiter wakes");
            let waker = Waker::from(Arc::clone(flag));
            let mut context = Context::from_waker(&waker);
            assert_eq!(future.as_mut().poll(&mut context), Poll::Ready(()));
        }
        assert!(cancellation.is_cancelled());
        assert!(cancellation.check().is_err());
    }
}
