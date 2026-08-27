//! macOS drag-out backend: a promised-file drag source built on AppKit.
//!
//! This is the real implementation behind [`crate::drag::DragSource`] on macOS.
//! It exists so the hero interaction of decision **D12** — pull a capture card
//! out of the stack and let go over Slack, Figma or a mail compose window —
//! behaves like a native file drag rather than a screenshot-shaped approximation.
//!
//! # The ownership chain (read this before changing anything)
//!
//! AppKit does *not* retain a drag source, and `NSFilePromiseProvider` holds its
//! delegate **weakly**. Getting these lifetimes wrong produces a crash that only
//! reproduces on a real drop, which is the worst possible bug to have in this
//! file. The arrangement is therefore deliberate:
//!
//! ```text
//! NSDraggingSession
//!   └─ retains NSDraggingItem
//!        └─ retains NSFilePromiseProvider        (its pasteboard writer)
//!             └─ userInfo (STRONG) ──▶ ScrozzDragPromise   ← our delegate,
//!                                       and the NSPasteboardItem data
//!                                       provider for the image flavours
//! ```
//!
//! One host object plays both roles — file-promise delegate *and* pasteboard
//! data provider — and it is anchored by the promise provider's `userInfo`,
//! which is a strong property. That anchor outlives
//! `draggingSession:endedAtPoint:operation:`, which matters: a receiver may ask
//! for the promised bytes *after* the drag has visually ended.
//!
//! Separately, [`LIVE`] holds the `NSDraggingSource` object for the duration of
//! the drag, because nothing else does. It is released in `endedAtPoint:`.
//!
//! # Threads
//!
//! Starting a drag is main-thread work, gated by [`crate::macos::main_thread`].
//! The promise *write* is not: AppKit runs it on the queue returned by
//! `operationQueueForFilePromiseProvider:`, so [`ScrozzDragPromise`]'s ivars are
//! deliberately plain thread-safe data (`String`, `Arc<dyn Fn … + Send + Sync>`)
//! plus an `NSOperationQueue`, which Foundation marks `Send + Sync`. Do not put
//! a `Retained<NSString>` in there.
//!
//! # Blocks without `block2`
//!
//! `filePromiseProvider:writePromiseToURL:completionHandler:` hands us an
//! Objective-C block. `scrozz-shell` does not depend on `block2` (see the crate
//! `Cargo.toml`), so the completion handler is invoked through the documented,
//! ABI-stable Blocks layout in [`invoke_completion`]. If `block2` is ever added
//! as a dependency, replace that with a typed `&DynBlock` parameter and delete
//! [`BlockHeader`].

use core::cell::RefCell;
use core::ffi::c_void;
use core::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{
    AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    sel,
};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSEventModifierFlags, NSEventType, NSFilePromiseProvider, NSImage,
    NSPasteboard, NSPasteboardItem, NSPasteboardItemDataProvider, NSPasteboardType,
    NSPasteboardTypePNG, NSPasteboardTypeTIFF, NSPasteboardWriting, NSView,
};
use objc2_foundation::{
    NSArray, NSData, NSError, NSObject, NSObjectProtocol, NSOperationQueue, NSPoint, NSRect,
    NSSize, NSString, NSURL,
};

use scrozz_core::{Error, Result};

use crate::drag::{
    ByteSource, DragCapability, DragOperation, DragOrigin, DragOutcome, DragPayload, DragSession,
    DragSource, card_rect_in_view, check_origin,
};
use crate::overlay::AppKitRect;

// ---------------------------------------------------------------------------
// Blocks ABI
// ---------------------------------------------------------------------------

/// The public prefix of an Objective-C block, per Clang's Block ABI.
///
/// Only these five fields are needed, and only `invoke` is read. The layout has
/// been fixed since the ABI was published; `block2::ffi::Block` models it
/// identically.
#[repr(C)]
struct BlockHeader {
    _isa: *const c_void,
    _flags: i32,
    _reserved: i32,
    invoke: Option<unsafe extern "C" fn(*mut c_void, *mut NSError)>,
    _descriptor: *const c_void,
}

/// Invokes an AppKit-supplied `void (^)(NSError *)` completion handler.
///
/// A null `block` is tolerated on purpose: it lets the write path be exercised
/// from a test without fabricating a block, and it is the right defensive
/// behaviour if AppKit ever passes nil.
///
/// # Safety
///
/// `block` must be null or a valid Objective-C block whose signature is
/// `void (^)(NSError * _Nullable)`. `error` must be null or a live `NSError`.
unsafe fn invoke_completion(block: *mut AnyObject, error: *mut NSError) {
    if block.is_null() {
        return;
    }
    // SAFETY: the caller guarantees `block` points at a block, whose leading
    // fields are the documented `BlockHeader` layout.
    let header = unsafe { &*block.cast::<BlockHeader>() };
    if let Some(invoke) = header.invoke {
        // SAFETY: `invoke` is the block's own function pointer and, per the
        // Blocks ABI, takes the block itself as its first argument.
        unsafe { invoke(block.cast::<c_void>(), error) };
    }
}

/// Builds an `NSError` in Scrozz's own domain so a failed promise surfaces a
/// readable reason in the receiving application instead of a silent no-op.
fn promise_error() -> Retained<NSError> {
    let domain = NSString::from_str("com.thatcube.scrozz.drag");
    // SAFETY: `errorWithDomain:code:userInfo:` accepts a nil userInfo; the
    // generic parameter is unconstrained because no dictionary is passed.
    unsafe { NSError::errorWithDomain_code_userInfo(&domain, 1, None) }
}

// ---------------------------------------------------------------------------
// The promise host
// ---------------------------------------------------------------------------

/// Everything the promise callbacks need, in plain thread-safe Rust data.
struct PromiseState {
    /// Final base name including extension, e.g. `Scrozz 2026-02-11.png`.
    file_name: String,
    /// Bytes for the file itself. Called only when a receiver accepts the drop.
    file_bytes: ByteSource,
    /// Bytes for the pasteboard image flavour, when one was offered.
    image_bytes: Option<ByteSource>,
    /// Background queue AppKit writes on, so a large encode does not stall the
    /// main thread at the moment of the drop.
    queue: Retained<NSOperationQueue>,
}

define_class!(
    /// File-promise delegate *and* lazy pasteboard data provider.
    ///
    /// Both roles live on one object so a single strong anchor
    /// (`NSFilePromiseProvider.userInfo`) keeps every lazy producer alive for
    /// exactly as long as anything can still ask for bytes.
    #[unsafe(super(NSObject))]
    #[ivars = PromiseState]
    struct ScrozzDragPromise;

    unsafe impl NSObjectProtocol for ScrozzDragPromise {}

    /// `NSFilePromiseProviderDelegate`, implemented with raw selectors.
    ///
    /// The typed conformance cannot be used: its required
    /// `writePromiseToURL:completionHandler:` method names `block2::DynBlock`,
    /// and `block2` is not a dependency of this crate. AppKit dispatches
    /// delegate messages by selector, so this is equivalent at runtime.
    impl ScrozzDragPromise {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name_for_type(
            &self,
            _provider: &NSFilePromiseProvider,
            _file_type: &NSString,
        ) -> Retained<NSString> {
            NSString::from_str(&self.ivars().file_name)
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn operation_queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write_promise(
            &self,
            _provider: &NSFilePromiseProvider,
            url: &NSURL,
            completion: *mut AnyObject,
        ) {
            match self.write_to(url) {
                Ok(()) => {
                    // SAFETY: `completion` is AppKit's block (or null); a nil
                    // NSError is the documented success signal.
                    unsafe { invoke_completion(completion, core::ptr::null_mut()) };
                }
                Err(err) => {
                    tracing::error!(error = %err, "drag: promised file could not be written");
                    let ns_error = promise_error();
                    // SAFETY: as above; `ns_error` is alive for the call.
                    unsafe {
                        invoke_completion(completion, Retained::as_ptr(&ns_error).cast_mut());
                    }
                }
            }
        }
    }

    /// Lazy image flavours (`public.png`, `public.tiff`).
    ///
    /// This protocol is block-free and not main-thread-only, so the typed
    /// conformance works and objc2 checks the signatures in debug builds.
    unsafe impl NSPasteboardItemDataProvider for ScrozzDragPromise {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide_data_for_type(
            &self,
            _pasteboard: Option<&NSPasteboard>,
            item: &NSPasteboardItem,
            requested: &NSPasteboardType,
        ) {
            self.provide_image(item, requested);
        }
    }
);

impl ScrozzDragPromise {
    /// Builds the host. Not main-thread-bound.
    fn new(
        file_name: String,
        file_bytes: ByteSource,
        image_bytes: Option<ByteSource>,
    ) -> Retained<Self> {
        let queue = NSOperationQueue::new();
        queue.setName(Some(&NSString::from_str("com.thatcube.scrozz.drag.promise")));
        queue.setMaxConcurrentOperationCount(1);

        let this = Self::alloc().set_ivars(PromiseState {
            file_name,
            file_bytes,
            image_bytes,
            queue,
        });
        // SAFETY: standard two-phase init of a class whose superclass is
        // NSObject; `set_ivars` has already populated our own storage.
        unsafe { msg_send![super(this), init] }
    }

    /// Produces the bytes and writes them at `url`. Runs on [`PromiseState::queue`].
    ///
    /// This is where "promise, don't pre-write" is actually honoured: nothing
    /// has touched the filesystem until this function runs, and it only runs
    /// because a receiving application accepted the drop and named a
    /// destination.
    fn write_to(&self, url: &NSURL) -> Result<()> {
        let bytes = (self.ivars().file_bytes)()?;

        let path = url
            .path()
            .ok_or_else(|| Error::Storage("drop destination has no filesystem path".into()))?
            .to_string();

        std::fs::write(&path, &bytes)?;
        tracing::debug!(bytes = bytes.len(), path = %path, "drag: promise fulfilled");
        Ok(())
    }

    /// Fills in a lazily requested image flavour on the pasteboard item.
    fn provide_image(&self, item: &NSPasteboardItem, requested: &NSPasteboardType) {
        let Some(source) = self.ivars().image_bytes.as_ref() else {
            return;
        };
        let png = match source() {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::error!(error = %err, "drag: image flavour unavailable");
                return;
            }
        };
        if png.is_empty() {
            tracing::error!("drag: image producer returned no bytes");
            return;
        }

        let data = NSData::with_bytes(&png);

        // SAFETY: reading AppKit's pasteboard-type globals. They are immortal
        // `NSString` constants initialised before any application code runs.
        let (png_type, tiff_type) = unsafe { (NSPasteboardTypePNG, NSPasteboardTypeTIFF) };

        if requested == png_type {
            item.setData_forType(&data, requested);
            return;
        }

        if requested == tiff_type {
            // Let AppKit transcode rather than adding an image codec to this
            // crate; `NSImage` reads PNG natively.
            let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
                tracing::error!("drag: NSImage could not decode the PNG for TIFF conversion");
                return;
            };
            match image.TIFFRepresentation() {
                Some(tiff) => {
                    item.setData_forType(&tiff, requested);
                }
                None => tracing::error!("drag: NSImage produced no TIFF representation"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The dragging source
// ---------------------------------------------------------------------------

thread_local! {
    /// Drag source objects AppKit is using but does not retain.
    ///
    /// Pushed when a drag begins, removed in
    /// `draggingSession:endedAtPoint:operation:`. Bounded in practice by the
    /// number of simultaneous drags, which a mouse caps at one.
    static LIVE: RefCell<Vec<Retained<ScrozzDragSourceObject>>> =
        const { RefCell::new(Vec::new()) };
}

define_class!(
    /// The `NSDraggingSource` for one drag, owning the [`DragSession`] handle
    /// the caller polls for an outcome.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DragSession]
    struct ScrozzDragSourceObject;

    unsafe impl NSObjectProtocol for ScrozzDragSourceObject {}

    unsafe impl NSDraggingSource for ScrozzDragSourceObject {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_operation_mask(
            &self,
            _session: &NSDraggingSession,
            context: NSDraggingContext,
        ) -> NSDragOperation {
            if context == NSDraggingContext::WithinApplication {
                // Dropping a card back onto our own overlay is not a move, it
                // is a cancel — and D21 says a cancel springs the card back.
                NSDragOperation::None
            } else {
                // `Copy` is the honest operation: per D14 nothing a drag does
                // may destroy the capture, so `Move` is never advertised.
                NSDragOperation::Copy | NSDragOperation::Generic
            }
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended_at_point(
            &self,
            _session: &NSDraggingSession,
            _point: NSPoint,
            operation: NSDragOperation,
        ) {
            self.ivars().finish(outcome_for(operation));
            retire(self);
        }
    }
);

impl ScrozzDragSourceObject {
    fn new(mtm: MainThreadMarker, session: DragSession) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(session);
        // SAFETY: standard two-phase init; ivars are already in place.
        unsafe { msg_send![super(this), init] }
    }
}

/// Translates AppKit's operation mask into a [`DragOutcome`].
///
/// macOS conflates "the receiver refused" with "the user let go over nothing":
/// both arrive as an empty mask. Both spring the card back under D21, so the
/// distinction is reported as [`DragOutcome::Cancelled`] and documented rather
/// than guessed at.
fn outcome_for(operation: NSDragOperation) -> DragOutcome {
    if operation.contains(NSDragOperation::Move) {
        DragOutcome::Accepted(DragOperation::Move)
    } else if operation.contains(NSDragOperation::Copy) {
        DragOutcome::Accepted(DragOperation::Copy)
    } else if operation.contains(NSDragOperation::Link) {
        DragOutcome::Accepted(DragOperation::Link)
    } else if operation.contains(NSDragOperation::Generic) {
        DragOutcome::Accepted(DragOperation::Generic)
    } else {
        DragOutcome::Cancelled
    }
}

/// Drops our keep-alive reference to a finished drag source.
fn retire(finished: &ScrozzDragSourceObject) {
    let target: *const ScrozzDragSourceObject = finished;
    LIVE.with(|live| {
        if let Ok(mut live) = live.try_borrow_mut() {
            live.retain(|held| !core::ptr::eq(Retained::as_ptr(held), target));
        }
    });
}

// ---------------------------------------------------------------------------
// The public backend
// ---------------------------------------------------------------------------

/// macOS implementation of [`DragSource`].
///
/// Construct with [`MacDragSource::new`] on the main thread; the type is `!Send`
/// by virtue of holding a `MainThreadMarker`.
#[derive(Debug, Clone)]
pub struct MacDragSource {
    mtm: MainThreadMarker,
}

impl MacDragSource {
    /// Creates the backend.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] off the main thread.
    pub fn new() -> Result<Self> {
        let mtm = crate::macos::main_thread("start a drag")?;
        Ok(Self { mtm })
    }

    /// Takes a strong reference to the `NSView` a drag originates from.
    ///
    /// # Safety
    ///
    /// `origin.surface()` must be a live `NSView *`.
    unsafe fn view_for(&self, origin: &DragOrigin) -> Result<Retained<NSView>> {
        let ptr = NonNull::new(origin.surface().as_ptr().cast::<NSView>())
            .ok_or_else(|| Error::InvalidRequest("drag origin has a null NSView".into()))?;
        // SAFETY: the caller guarantees the pointer is a live NSView. Taking our
        // own strong reference stops AppKit deallocating it mid-drag.
        let view = unsafe { Retained::retain(ptr.as_ptr()) }
            .ok_or_else(|| Error::TargetGone("drag origin view could not be retained".into()))?;
        Ok(view)
    }

    /// The event a drag must be attached to.
    ///
    /// AppKit wants the mouse event that initiated the gesture. `scrozz-ui`
    /// classifies gestures itself, so the current event is used when it is a
    /// mouse event, and one is synthesised at the pointer otherwise.
    fn drag_event(&self, view: &NSView, pointer_in_view: NSPoint) -> Result<Retained<NSEvent>> {
        let app = NSApplication::sharedApplication(self.mtm);
        if let Some(current) = app.currentEvent()
            && matches!(
                current.r#type(),
                NSEventType::LeftMouseDown
                    | NSEventType::LeftMouseDragged
                    | NSEventType::RightMouseDown
                    | NSEventType::RightMouseDragged
                    | NSEventType::OtherMouseDown
                    | NSEventType::OtherMouseDragged
            )
        {
            return Ok(current);
        }

        let window = view
            .window()
            .ok_or_else(|| Error::TargetGone("drag origin view is not in a window".into()))?;
        let in_window = view.convertPoint_toView(pointer_in_view, None);

        // `mouseEventWithType:…` is a safe binding: a nil graphics context is
        // documented, and all numeric arguments are in range. Synthesising is
        // the supported route to an event for
        // `beginDraggingSessionWithItems:event:source:` when the real one is
        // unavailable.
        let event =
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                NSEventType::LeftMouseDragged,
                in_window,
                NSEventModifierFlags::empty(),
                0.0,
                window.windowNumber(),
                None,
                0,
                1,
                1.0,
            );
        event.ok_or_else(|| Error::Platform("could not synthesise a drag event".into()))
    }

    /// The dragging item carrying the promised file.
    fn promise_item(
        &self,
        payload: &DragPayload,
        host: &Retained<ScrozzDragPromise>,
        card: AppKitRect,
    ) -> Retained<NSDraggingItem> {
        let provider = NSFilePromiseProvider::new();
        provider.setFileType(&NSString::from_str(payload.file().format().uti()));

        // SAFETY: `setDelegate:` stores an *unretained* reference to our host,
        // which is exactly why the strong `userInfo` anchor below is mandatory.
        unsafe {
            let _: () = msg_send![&*provider, setDelegate: &**host];
        }
        // SAFETY: `userInfo` is a strong `id` property. Storing the delegate in
        // it is the ownership anchor described in this module's header.
        unsafe {
            let anchor: &AnyObject = host;
            provider.setUserInfo(Some(anchor));
        }

        let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*provider);
        let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
        self.decorate(&item, payload.preview_png(), card);
        item
    }

    /// The dragging item carrying lazy image flavours.
    fn image_item(
        &self,
        host: &Retained<ScrozzDragPromise>,
        card: AppKitRect,
    ) -> Retained<NSDraggingItem> {
        let item = NSPasteboardItem::new();
        // SAFETY: reading AppKit's immortal pasteboard-type globals.
        let types = unsafe { NSArray::from_slice(&[NSPasteboardTypePNG, NSPasteboardTypeTIFF]) };
        let provider: &ProtocolObject<dyn NSPasteboardItemDataProvider> =
            ProtocolObject::from_ref(&**host);
        item.setDataProvider_forTypes(provider, &types);

        let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*item);
        let dragging = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
        // Only the promise item shows a preview; this one rides invisibly.
        self.decorate(&dragging, None, card);
        dragging
    }

    /// Gives a dragging item its on-screen frame and, when one was supplied, the
    /// preview image that follows the pointer.
    fn decorate(&self, item: &NSDraggingItem, preview_png: Option<&[u8]>, card: AppKitRect) {
        let frame = NSRect::new(
            NSPoint::new(card.x, card.y),
            NSSize::new(card.width, card.height),
        );

        let contents = preview_png.and_then(|png| {
            let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(png));
            if image.is_none() {
                tracing::warn!("drag: preview PNG could not be decoded; dragging without an image");
            }
            image
        });

        // SAFETY: `setDraggingFrame:contents:` accepts any object AppKit can
        // draw, or nil. Only an `NSImage` or nothing is passed.
        unsafe {
            match contents {
                Some(image) => {
                    let object: &AnyObject = &image;
                    item.setDraggingFrame_contents(frame, Some(object));
                }
                None => item.setDraggingFrame_contents(frame, None),
            }
        }
    }
}

impl DragSource for MacDragSource {
    fn name(&self) -> &str {
        "macOS/AppKit"
    }

    fn capability(&self) -> DragCapability {
        DragCapability::FULL
    }

    fn begin(&self, payload: DragPayload, origin: DragOrigin) -> Result<DragSession> {
        check_origin(&origin)?;

        // SAFETY: `DragOrigin::surface` is documented to carry a live NSView
        // supplied by the overlay, which outlives the drag it starts.
        let view = unsafe { self.view_for(&origin)? };

        let bounds = view.bounds();
        let card = card_rect_in_view(origin.card(), bounds.size.height, view.isFlipped());

        let host = ScrozzDragPromise::new(
            payload.file().file_name(),
            payload.file().byte_source(),
            payload.image().cloned(),
        );

        let mut items = Vec::with_capacity(2);
        items.push(self.promise_item(&payload, &host, card));
        if payload.image().is_some() {
            items.push(self.image_item(&host, card));
        }
        let item_count = items.len();

        let session = DragSession::new();
        let source = ScrozzDragSourceObject::new(self.mtm, session.clone());
        LIVE.with(|live| live.borrow_mut().push(source.clone()));

        let pointer = NSPoint::new(origin.pointer().x, origin.pointer().y);
        let event = match self.drag_event(&view, pointer) {
            Ok(event) => event,
            Err(err) => {
                retire(&source);
                return Err(err);
            }
        };

        let array = NSArray::from_retained_slice(&items);
        let protocol: &ProtocolObject<dyn NSDraggingSource> = ProtocolObject::from_ref(&*source);
        let dragging = view.beginDraggingSessionWithItems_event_source(&array, &event, protocol);
        dragging.setAnimatesToStartingPositionsOnCancelOrFail(true);

        tracing::info!(
            file = %payload.file().file_name(),
            items = item_count,
            "drag: session began"
        );
        Ok(session)
    }
}

// ---------------------------------------------------------------------------
// Test-only seams
// ---------------------------------------------------------------------------

/// Test seam: build the promise host and drive its callbacks without a drag.
///
/// This exists so the parts that *can* be verified on a machine whose mouse is
/// in use — filename generation, the promise producing correct bytes, image
/// flavour negotiation — actually are. Nothing here touches a window, the
/// system pasteboard, or the pointer.
#[doc(hidden)]
pub mod test_support {
    use super::{
        ClassType, DragPayload, NSFilePromiseProvider, NSPasteboardItem, NSString, NSURL, NSView,
        Result, Retained, ScrozzDragPromise, msg_send, sel,
    };

    /// A standalone promise host, exactly as [`super::MacDragSource::begin`]
    /// would build it.
    pub struct PromiseHarness {
        host: Retained<ScrozzDragPromise>,
    }

    impl PromiseHarness {
        /// Builds a harness for `payload`.
        #[must_use]
        pub fn new(payload: &DragPayload) -> Self {
            Self {
                host: ScrozzDragPromise::new(
                    payload.file().file_name(),
                    payload.file().byte_source(),
                    payload.image().cloned(),
                ),
            }
        }

        /// The filename AppKit would be told to use, fetched through the real
        /// Objective-C dispatch path.
        #[must_use]
        pub fn file_name(&self) -> String {
            let uti = NSString::from_str("public.png");
            let provider = NSFilePromiseProvider::new();
            // SAFETY: messaging our own delegate method with live arguments. The
            // selector matches the implementation's declared signature.
            let name: Retained<NSString> = unsafe {
                msg_send![&*self.host, filePromiseProvider: &*provider, fileNameForType: &*uti]
            };
            name.to_string()
        }

        /// Runs the promise write against `path`, as a drop would.
        ///
        /// # Errors
        ///
        /// Propagates whatever the byte producer or the filesystem returned.
        pub fn write_to(&self, path: &std::path::Path) -> Result<()> {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            self.host.write_to(&url)
        }

        /// Asks for one image flavour and reads back what landed on the item.
        #[must_use]
        pub fn image_flavour(&self, uti: &str) -> Option<Vec<u8>> {
            let item = NSPasteboardItem::new();
            let requested = NSString::from_str(uti);
            self.host.provide_image(&item, &requested);
            item.dataForType(&requested).map(|data| data.to_vec())
        }
    }

    /// Whether `NSView` implements the drag entry point at all.
    ///
    /// This answers only the part that is checkable without a mouse. Whether a
    /// *non-activating* panel can host a real drop still needs a human.
    #[must_use]
    pub fn view_can_begin_drags() -> bool {
        NSView::class().responds_to(sel!(beginDraggingSessionWithItems:event:source:))
    }
}
