//! A minimal Objective-C block, hand-rolled, and a blocking bridge across it.
//!
//! # Why this exists
//!
//! Every entry point into ScreenCaptureKit that matters here —
//! `+[SCShareableContent getShareableContentWithCompletionHandler:]` and
//! `+[SCScreenshotManager captureImageWithFilter:configuration:completionHandler:]`
//! — is completion-handler based, and the handler is an Objective-C block.
//!
//! The natural way to build one is the `block2` crate. It is **not** a declared
//! dependency of `scrozz-capture`; it arrives only transitively, inside
//! `objc2-screen-capture-kit`'s own namespace. So `block2::DynBlock` cannot be
//! named here, and the generated bindings whose signatures mention it cannot be
//! called at all. Rather than change the manifest, this module implements the
//! block literal directly and dispatches through `objc2::msg_send!`.
//!
//! The [Block ABI][abi] is stable and public, and this is the smallest usable
//! subset of it:
//!
//! - The literal is a stack block with **no** copy/dispose helpers. Its only
//!   capture is a raw pointer, so `_Block_copy` — which ScreenCaptureKit calls
//!   when it stores the handler — is a plain `malloc` plus `memcpy`. Nothing
//!   needs retaining, so no helpers are required and none are advertised.
//! - `BLOCK_HAS_SIGNATURE` is deliberately not set. That selects the original
//!   10.6 layout, whose descriptor is just `{ reserved, size }`. `block2` does
//!   the same for its global blocks, and ScreenCaptureKit invokes the handler
//!   directly rather than reflecting on it.
//!
//! # Lifetimes across the callback
//!
//! Two hazards make this more than a pointer hand-off, and both are handled
//! here rather than at the call sites.
//!
//! The rendezvous outliving the waiter is the first. The caller cannot simply
//! put a [`Slot`] on its stack, because after a timeout it would return and
//! drop that frame while ScreenCaptureKit still holds a block pointing into it.
//! The slot is therefore an [`Arc`], and the block owns a strong reference that
//! its `invoke` consumes exactly once. A handler that never fires leaks one
//! small allocation; a handler that fires late finds the slot still alive.
//!
//! The delivered objects outliving the handler is the second. They arrive
//! autoreleased, and the pool can drain the moment the handler returns — which
//! may be before the woken thread has looked at them. `invoke` therefore
//! retains both before publishing them, and the slot owns those `+1` references
//! until someone takes or drops them.
//!
//! [abi]: https://clang.llvm.org/docs/Block-ABI-Apple.html

use std::ffi::{c_ulong, c_void};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use objc2::encode::{Encode, Encoding, RefEncode};

unsafe extern "C" {
    /// The class every stack-allocated block's `isa` points at.
    ///
    /// Typed as an opaque byte array rather than a struct: only its address is
    /// ever used, and inventing a layout for it would be a lie.
    static _NSConcreteStackBlock: [*const c_void; 32];

    fn objc_retain(object: *mut c_void) -> *mut c_void;
    fn objc_release(object: *mut c_void);
}

/// Descriptor for a block with neither copy/dispose helpers nor a signature.
#[repr(C)]
struct BlockDescriptor {
    reserved: c_ulong,
    size: c_ulong,
}

// SAFETY: the descriptor is immutable data with no interior mutability, and the
// Objective-C runtime only ever reads it.
unsafe impl Sync for BlockDescriptor {}

static DESCRIPTOR: BlockDescriptor = BlockDescriptor {
    reserved: 0,
    size: size_of::<CompletionBlock>() as c_ulong,
};

/// A block literal taking two pointer arguments and returning nothing.
///
/// That is the shape of every ScreenCaptureKit completion handler used here:
/// `^(id result, NSError *error)`.
#[repr(C)]
pub(crate) struct CompletionBlock {
    isa: *const c_void,
    flags: i32,
    reserved: i32,
    invoke: unsafe extern "C" fn(*mut CompletionBlock, *mut c_void, *mut c_void),
    descriptor: *const BlockDescriptor,
    /// The single captured variable: an owned strong reference to the slot,
    /// consumed by `invoke`.
    slot: *const Slot,
}

/// A `*mut c_void` that the Objective-C runtime will read as a block (`@?`).
///
/// `objc2`'s `msg_send!` verifies argument encodings against the runtime's own
/// method signature in debug builds. A bare `*mut c_void` encodes as `^v` and
/// is rejected where a block is expected, so the pointer is wrapped in a
/// newtype that declares the correct encoding.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub(crate) struct BlockArg(pub(crate) *mut c_void);

// SAFETY: `BlockArg` is a `#[repr(transparent)]` pointer passed where the
// Objective-C runtime expects a block pointer, which is what `Encoding::Block`
// describes.
unsafe impl Encode for BlockArg {
    const ENCODING: Encoding = Encoding::Block;
}

// SAFETY: as above, with one more level of indirection.
unsafe impl RefEncode for BlockArg {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

/// The two `id`-shaped values a completion handler delivers, each retained `+1`.
#[derive(Clone, Copy)]
pub(crate) struct Outcome {
    /// The successful result, or null.
    pub(crate) value: *mut c_void,
    /// The `NSError`, or null.
    pub(crate) error: *mut c_void,
}

// SAFETY: `Outcome` is a pair of Objective-C object pointers. Objective-C
// objects are not bound to the thread that produced them, and the handoff is a
// single move from the completion-handler thread to the waiting thread with a
// mutex in between, so there is no aliasing.
unsafe impl Send for Outcome {}

impl Outcome {
    /// Releases both references. Used when nobody claimed them.
    pub(crate) fn release(self) {
        for object in [self.value, self.error] {
            if !object.is_null() {
                // SAFETY: each pointer was retained `+1` in `invoke_completion`
                // and has not been released since.
                unsafe { objc_release(object) };
            }
        }
    }
}

enum State {
    /// Nobody has fired and nobody has given up.
    Pending,
    /// The handler fired; the outcome's `+1` references are owned here.
    Ready(Outcome),
    /// The waiter took the outcome, or gave up waiting for it.
    Taken,
}

/// A rendezvous between a completion handler and the thread that is waiting.
pub(crate) struct Slot {
    state: Mutex<State>,
    ready: Condvar,
}

impl Slot {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::Pending),
            ready: Condvar::new(),
        })
    }

    /// Blocks until the handler fires, or until `timeout` elapses.
    ///
    /// A timeout is a genuine possibility rather than paranoia: the handler is
    /// delivered on one of ScreenCaptureKit's own queues, and a wedged capture
    /// daemon must surface as an error rather than hang the caller forever.
    ///
    /// After this returns the slot is closed: a late handler releases what it
    /// was carrying instead of publishing it.
    pub(crate) fn wait(&self, timeout: Duration) -> Option<Outcome> {
        let guard = lock(&self.state);
        let (mut guard, _) = self
            .ready
            .wait_timeout_while(guard, timeout, |state| matches!(state, State::Pending))
            .unwrap_or_else(PoisonError::into_inner);

        match std::mem::replace(&mut *guard, State::Taken) {
            State::Ready(outcome) => Some(outcome),
            State::Pending | State::Taken => None,
        }
    }

    fn fulfil(&self, outcome: Outcome) {
        let mut guard = lock(&self.state);
        match *guard {
            State::Pending => {
                *guard = State::Ready(outcome);
                self.ready.notify_all();
            }
            // The waiter timed out and moved on; nobody will ever read this.
            State::Ready(_) | State::Taken => {
                drop(guard);
                outcome.release();
            }
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        // A capture that arrived just too late still holds a CGImage, which can
        // be tens of megabytes. Release it rather than leak it.
        let mut guard = lock(&self.state);
        if let State::Ready(outcome) = std::mem::replace(&mut *guard, State::Taken) {
            drop(guard);
            outcome.release();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned slot is still perfectly readable: it holds two raw pointers
    // and no invariant a panic could have broken.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The block's `invoke` function.
///
/// Declared `extern "C"` rather than `extern "C-unwind"` so that a panic here
/// aborts instead of unwinding into Objective-C frames. The body is written not
/// to panic in the first place.
unsafe extern "C" fn invoke_completion(
    block: *mut CompletionBlock,
    value: *mut c_void,
    error: *mut c_void,
) {
    // SAFETY: `block` is the literal this function was installed in, or a
    // `_Block_copy` of it, which preserves the captured `slot` verbatim.
    let slot = unsafe { (*block).slot };
    if slot.is_null() {
        return;
    }

    // The arguments arrive autoreleased. Retain before publishing, so the
    // waiting thread cannot observe a pool drain underneath it.
    let retain = |object: *mut c_void| {
        if object.is_null() {
            object
        } else {
            // SAFETY: a live Objective-C object; null was filtered out.
            unsafe { objc_retain(object) }
        }
    };
    let outcome = Outcome {
        value: retain(value),
        error: retain(error),
    };

    // SAFETY: the block captured an owned strong reference, created by
    // `Arc::into_raw` in `CompletionBlock::new`. Taking it back here consumes
    // it exactly once, because a completion handler is invoked at most once.
    let slot = unsafe { Arc::from_raw(slot) };
    slot.fulfil(outcome);
}

impl CompletionBlock {
    /// Builds a block literal that fulfils `slot` when invoked.
    ///
    /// The literal owns a strong reference to `slot`; if it is dropped without
    /// ever being invoked, that reference leaks. Every caller here passes it
    /// straight to ScreenCaptureKit, which always invokes it.
    pub(crate) fn new(slot: &Arc<Slot>) -> Self {
        Self {
            isa: (&raw const _NSConcreteStackBlock).cast(),
            // No copy/dispose helpers and no signature: the 10.6 layout, whose
            // descriptor is exactly `DESCRIPTOR`'s two fields.
            flags: 0,
            reserved: 0,
            invoke: invoke_completion,
            descriptor: &raw const DESCRIPTOR,
            slot: Arc::into_raw(Arc::clone(slot)),
        }
    }

    /// The pointer to hand to Objective-C.
    pub(crate) fn as_arg(&mut self) -> BlockArg {
        BlockArg(std::ptr::from_mut(self).cast())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal must match the ABI's expected header layout exactly, or the
    /// runtime reads `invoke` from the wrong offset and jumps into nothing.
    #[test]
    fn literal_matches_the_block_abi_layout() {
        assert_eq!(std::mem::offset_of!(CompletionBlock, isa), 0);
        assert_eq!(std::mem::offset_of!(CompletionBlock, flags), 8);
        assert_eq!(std::mem::offset_of!(CompletionBlock, reserved), 12);
        assert_eq!(std::mem::offset_of!(CompletionBlock, invoke), 16);
        assert_eq!(std::mem::offset_of!(CompletionBlock, descriptor), 24);
        assert_eq!(std::mem::offset_of!(CompletionBlock, slot), 32);
        assert_eq!(DESCRIPTOR.size as usize, size_of::<CompletionBlock>());
    }

    /// Invoking the block by hand exercises the same path ScreenCaptureKit
    /// takes, without needing a screen or a permission grant.
    #[test]
    fn invoking_the_block_fulfils_the_slot() {
        let slot = Slot::new();
        let mut block = CompletionBlock::new(&slot);
        let arg = block.as_arg();

        // A real Objective-C object, so the retain inside `invoke` is valid.
        let object = objc2_foundation::NSString::from_str("captured");
        let raw = objc2::rc::Retained::as_ptr(&object)
            .cast::<c_void>()
            .cast_mut();

        // SAFETY: `arg` points at `block`, which is alive for this scope, and
        // `raw` is a live object.
        unsafe { (block.invoke)(arg.0.cast(), raw, std::ptr::null_mut()) };

        let outcome = slot.wait(Duration::from_millis(0)).expect("fulfilled");
        assert_eq!(outcome.value, raw);
        assert!(outcome.error.is_null());
        outcome.release();
    }

    #[test]
    fn waiting_without_a_handler_times_out_rather_than_hanging() {
        let slot = Slot::new();
        assert!(slot.wait(Duration::from_millis(10)).is_none());
        assert_eq!(Arc::strong_count(&slot), 1);
    }

    #[test]
    fn a_block_holds_the_slot_alive_past_the_waiter() {
        let slot = Slot::new();
        let block = CompletionBlock::new(&slot);
        assert_eq!(Arc::strong_count(&slot), 2);

        // Reclaim the reference the block owns, so the test does not leak.
        // SAFETY: `block.slot` came from `Arc::into_raw` and is taken once.
        drop(unsafe { Arc::from_raw(block.slot) });
    }
}
