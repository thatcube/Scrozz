//! macOS drag-out backend: a real file drag built on AppKit.
//!
//! This is the implementation behind [`crate::drag::DragSource`] on macOS. It
//! exists so the hero interaction of decision **D12** — pull a capture card out
//! of the stack and let go over Slack, Figma or a mail compose window — behaves
//! like a native file drag rather than a screenshot-shaped approximation.
//!
//! # Why this drags a file URL and not a file promise
//!
//! `NSFilePromiseProvider` is the API Apple documents for exactly this, and it
//! is the one this backend used first. Finder, Mail and TextEdit honour it.
//! Chromium does not — it reads `public.file-url` and ignores promises — so
//! every Electron application (Slack, Discord, VS Code, Notion) and every
//! browser drop zone refuses a promise-only drag *silently*. Those are the
//! destinations D12 names as the reason drag-out exists, so a promise-only
//! drag is a drag that visibly does nothing in the places that matter.
//!
//! So one `NSPasteboardItem` is offered, carrying three flavours in preference
//! order:
//!
//! | Type | When it is filled | Who reads it |
//! |---|---|---|
//! | `public.file-url` | eagerly, at drag start | Finder, Slack, Discord, browsers, VS Code, Mail |
//! | `public.png` | eagerly, from the same bytes | Figma, Notes, Keynote, Preview, rich text |
//! | `public.tiff` | lazily, transcoded on request | older AppKit receivers |
//!
//! **One item, not two.** A second dragging item for the image flavours would
//! make Finder create two files from one drop.
//!
//! The file that URL points at is written once, at drag start, and its lifetime
//! belongs to [`crate::drag::artifact::DragArtifact`] — which is also why this
//! file does not delete anything in `draggingSession:endedAtPoint:operation:`.
//!
//! # The ownership chain (read this before changing anything)
//!
//! AppKit does *not* retain a drag source, and an `NSPasteboardItem` does not
//! documentably retain its data provider. Getting these lifetimes wrong
//! produces a crash that only reproduces on a real drop, which is the worst
//! possible bug to have in this file. The arrangement is therefore deliberate:
//!
//! ```text
//! LIVE (thread-local) ──▶ ScrozzDragSourceObject   ← AppKit holds this weakly
//!                            └─ ivars: DragSession (outcome + artifact)
//!                                      Retained<ScrozzImageProvider>
//!
//! PROVIDERS (thread-local) ──▶ ScrozzImageProvider ← the pasteboard item's
//!                                                    data provider, which may
//!                                                    be asked for bytes after
//!                                                    the drag has ended
//! ```
//!
//! `LIVE` is emptied in `draggingSession:endedAtPoint:operation:`.
//! `PROVIDERS` is emptied in `pasteboardFinishedWithDataProvider:` and, as a
//! backstop for a receiver that never triggers it, is capped at
//! `MAX_LIVE_PROVIDERS` so nothing accumulates across a session.
//!
//! Because `public.file-url` and `public.png` are both set *eagerly*, a lost
//! provider degrades to "no TIFF flavour" rather than to a failed drop.
//!
//! # Threads
//!
//! Starting a drag is main-thread work, gated by `crate::macos::main_thread`.
//! A lazy flavour request is not guaranteed to arrive there, so
//! `ScrozzImageProvider`'s ivars are deliberately plain thread-safe data.

use core::cell::RefCell;
use core::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{
    AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    sel,
};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSEventModifierFlags, NSEventType, NSImage, NSPasteboard,
    NSPasteboardItem, NSPasteboardItemDataProvider, NSPasteboardType, NSPasteboardTypeFileURL,
    NSPasteboardTypePNG, NSPasteboardTypeTIFF, NSPasteboardWriting, NSView,
};
use objc2_foundation::{
    NSArray, NSData, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
};

use scrozz_core::{Error, Result};

use crate::drag::artifact::artifact_root;
use crate::drag::{
    DragCapability, DragOperation, DragOrigin, DragOutcome, DragPayload, DragPreview, DragSession,
    DragSource, card_rect_in_view, check_origin, preview_hotspot,
};
use crate::overlay::AppKitRect;

/// How many finished image providers are kept alive as a backstop.
///
/// A receiver that reads a lazy flavour after the drop needs the provider to
/// still exist; `pasteboardFinishedWithDataProvider:` is the documented signal
/// that it no longer does, but a receiver that never asks for a lazy type never
/// triggers it. Holding the last few is bounded, cheap, and removes an entire
/// class of use-after-free from this file.
const MAX_LIVE_PROVIDERS: usize = 4;

// ---------------------------------------------------------------------------
// The image provider
// ---------------------------------------------------------------------------

/// Everything the lazy flavour callback needs, in plain thread-safe Rust data.
struct ProviderState {
    /// PNG bytes of the capture, already produced for the file that was
    /// written. Shared rather than re-encoded.
    png: std::sync::Arc<Vec<u8>>,
}

define_class!(
    /// Lazy pasteboard data provider for the flavours that cost something to
    /// make.
    ///
    /// Only `public.tiff` is served here. `public.file-url` and `public.png`
    /// are set eagerly on the item, because they are the two a drop actually
    /// depends on and because eager data cannot dangle.
    #[unsafe(super(NSObject))]
    #[ivars = ProviderState]
    struct ScrozzImageProvider;

    unsafe impl NSObjectProtocol for ScrozzImageProvider {}

    /// This protocol is block-free and not main-thread-only, so the typed
    /// conformance works and objc2 checks the signatures in debug builds.
    unsafe impl NSPasteboardItemDataProvider for ScrozzImageProvider {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide_data_for_type(
            &self,
            _pasteboard: Option<&NSPasteboard>,
            item: &NSPasteboardItem,
            requested: &NSPasteboardType,
        ) {
            self.provide_image(item, requested);
        }

        #[unsafe(method(pasteboardFinishedWithDataProvider:))]
        fn finished(&self, _pasteboard: &NSPasteboard) {
            retire_provider(self);
        }
    }
);

impl ScrozzImageProvider {
    /// Builds the provider. Not main-thread-bound.
    fn new(png: std::sync::Arc<Vec<u8>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ProviderState { png });
        // SAFETY: standard two-phase init of a class whose superclass is
        // NSObject; `set_ivars` has already populated our own storage.
        unsafe { msg_send![super(this), init] }
    }

    /// Fills in a lazily requested image flavour on the pasteboard item.
    fn provide_image(&self, item: &NSPasteboardItem, requested: &NSPasteboardType) {
        let png = &self.ivars().png;
        if png.is_empty() {
            tracing::error!("drag: no capture bytes to transcode");
            return;
        }

        // SAFETY: reading AppKit's pasteboard-type globals. They are immortal
        // `NSString` constants initialised before any application code runs.
        let (png_type, tiff_type) = unsafe { (NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
        let data = NSData::with_bytes(png);

        if requested == png_type {
            item.setData_forType(&data, requested);
            return;
        }

        if requested == tiff_type {
            // Let AppKit transcode rather than adding an image codec to this
            // crate; `NSImage` reads PNG natively. Deliberately lazy: a 4K
            // capture is roughly 30 MB as TIFF, which is not something to spend
            // on the main thread at the instant a drag starts.
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

thread_local! {
    /// Image providers that may still be asked for bytes.
    ///
    /// See `MAX_LIVE_PROVIDERS` for why this is a bounded queue rather than a
    /// strict lifetime.
    static PROVIDERS: RefCell<Vec<Retained<ScrozzImageProvider>>> =
        const { RefCell::new(Vec::new()) };
}

/// Keeps `provider` alive, evicting the oldest once the cap is reached.
fn hold_provider(provider: &Retained<ScrozzImageProvider>) {
    PROVIDERS.with(|held| {
        if let Ok(mut held) = held.try_borrow_mut() {
            held.push(provider.clone());
            while held.len() > MAX_LIVE_PROVIDERS {
                held.remove(0);
            }
        }
    });
}

/// Releases a provider AppKit has said it is finished with.
fn retire_provider(finished: &ScrozzImageProvider) {
    let target: *const ScrozzImageProvider = finished;
    PROVIDERS.with(|held| {
        if let Ok(mut held) = held.try_borrow_mut() {
            held.retain(|item| !core::ptr::eq(Retained::as_ptr(item), target));
        }
    });
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

/// What one drag's source object owns.
struct SourceState {
    /// The handle the caller polls, which also owns the temporary file.
    session: DragSession,
    /// Kept so the provider outlives the drag even if AppKit does not retain
    /// it; see the module header.
    _provider: Retained<ScrozzImageProvider>,
}

define_class!(
    /// The `NSDraggingSource` for one drag, owning the [`DragSession`] handle
    /// the caller polls for an outcome.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = SourceState]
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
            // This records the outcome *and* decides the fate of the temporary
            // file. It deliberately does not delete anything an accepted
            // receiver may still be reading — see `DragArtifact`.
            self.ivars().session.finish(outcome_for(operation));
            retire(self);
        }
    }
);

impl ScrozzDragSourceObject {
    fn new(
        mtm: MainThreadMarker,
        session: DragSession,
        provider: Retained<ScrozzImageProvider>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SourceState {
            session,
            _provider: provider,
        });
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

/// Builds the one pasteboard item a capture drag offers.
///
/// Type order is preference order: a receiver that understands more than one
/// flavour takes the first it recognises, and for a screenshot that should be
/// the file.
fn capture_item(
    path: &std::path::Path,
    png: &[u8],
    provider: &Retained<ScrozzImageProvider>,
) -> Result<Retained<NSPasteboardItem>> {
    let item = NSPasteboardItem::new();

    // SAFETY: reading AppKit's immortal pasteboard-type globals.
    let (file_url_type, png_type, tiff_type) = unsafe {
        (
            NSPasteboardTypeFileURL,
            NSPasteboardTypePNG,
            NSPasteboardTypeTIFF,
        )
    };

    let native_path = NSString::from_str(&path.to_string_lossy());
    let url = NSURL::fileURLWithPath(&native_path);
    let absolute = url.absoluteString().ok_or_else(|| {
        Error::Platform("the drag file could not be expressed as a URL".to_owned())
    })?;
    if !item.setString_forType(&absolute, file_url_type) {
        return Err(Error::Platform(
            "the pasteboard refused the drag file URL".to_owned(),
        ));
    }

    // Eager, from bytes that already exist: a receiver that wants image data
    // gets it without a callback that could outlive its provider.
    //
    // Empty means the payload offered no image — an MP4 or a JPEG capture. Both
    // image flavours are then withheld together, because a `public.tiff` the
    // provider cannot fill is a type the receiver will ask for and get nothing
    // from, which reads to it as a failed drop rather than an absent flavour.
    if png.is_empty() {
        return Ok(item);
    }

    item.setData_forType(&NSData::with_bytes(png), png_type);

    let types = NSArray::from_slice(&[tiff_type]);
    let as_protocol: &ProtocolObject<dyn NSPasteboardItemDataProvider> =
        ProtocolObject::from_ref(&**provider);
    item.setDataProvider_forTypes(as_protocol, &types);

    Ok(item)
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
    /// AppKit wants the mouse event that initiated the gesture, and it must
    /// still be a *held* button: a session begun after the mouse came up ends
    /// immediately, which is exactly what "the drag animates and nothing ever
    /// drops" looks like. `scrozz-ui` therefore reports its drag-out intent
    /// mid-gesture rather than at release.
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
        // unavailable — for instance when the frame that noticed the gesture
        // was driven by a timer rather than by the mouse event itself.
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

    /// Gives a dragging item its on-screen frame and, when one was supplied, the
    /// preview image that follows the pointer.
    fn decorate(item: &NSDraggingItem, preview: Option<&DragPreview>, frame: AppKitRect) {
        let frame = NSRect::new(
            NSPoint::new(frame.x, frame.y),
            NSSize::new(frame.width, frame.height),
        );

        let contents = preview.and_then(|preview| {
            let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(preview.png()));
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

    fn preview_frame(
        card: AppKitRect,
        logical_card: scrozz_core::LogicalRect,
        pointer: scrozz_core::LogicalPoint,
        preview: &DragPreview,
        flipped: bool,
    ) -> AppKitRect {
        let grab = scrozz_core::LogicalPoint::new(
            pointer.x - logical_card.origin.x,
            pointer.y - logical_card.origin.y,
        );
        let hotspot = preview_hotspot(logical_card.size, grab, preview.size());
        let pointer_x = card.x + grab.x;
        let pointer_y = if flipped {
            card.y + grab.y
        } else {
            card.y + card.height - grab.y
        };
        let y = if flipped {
            pointer_y - hotspot.y
        } else {
            pointer_y - (preview.size().height - hotspot.y)
        };
        AppKitRect::new(
            pointer_x - hotspot.x,
            y,
            preview.size().width,
            preview.size().height,
        )
    }
}

impl DragSource for MacDragSource {
    fn name(&self) -> &str {
        "macOS/AppKit"
    }

    fn capability(&self) -> DragCapability {
        DragCapability::EAGER_FILE_AND_IMAGE
    }

    fn begin(&self, payload: DragPayload, origin: DragOrigin) -> Result<DragSession> {
        check_origin(&origin)?;

        // SAFETY: `DragOrigin::surface` is documented to carry a live NSView
        // supplied by the overlay, which outlives the drag it starts.
        let view = unsafe { self.view_for(&origin)? };

        let bounds = view.bounds();
        let logical_card = origin.card();
        let flipped = view.isFlipped();
        let card = card_rect_in_view(logical_card, bounds.size.height, flipped);

        // The one encode this whole feature costs, and the only write. If it
        // fails, nothing native has been touched yet.
        let (artifact, bytes) = payload.materialise(&artifact_root())?;
        let file_bytes = std::sync::Arc::new(bytes);

        // Image flavours come from the image producer, never from the file
        // bytes. For a screenshot the two are the same `Arc` and this costs
        // nothing; for an MP4 or a JPEG there is no image to offer and this is
        // the difference between "no PNG flavour" and "a PNG flavour that is
        // not a PNG".
        let png = payload
            .image_png(&file_bytes)?
            .unwrap_or_else(|| std::sync::Arc::new(Vec::new()));

        let session = DragSession::new();
        // Attached before anything can fail, so every exit path from here has an
        // owner that will delete the file.
        session.attach_artifact(artifact);

        let provider = ScrozzImageProvider::new(std::sync::Arc::clone(&png));
        hold_provider(&provider);

        let source = ScrozzDragSourceObject::new(self.mtm, session.clone(), provider.clone());
        LIVE.with(|live| live.borrow_mut().push(source.clone()));

        let started = (|| -> Result<()> {
            let path = session
                .artifact_path()
                .ok_or_else(|| Error::Platform("the drag file went missing".to_owned()))?;
            let item = capture_item(&path, &png, &provider)?;

            let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*item);
            let dragging =
                NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writer);
            let frame = payload.preview().map_or(card, |preview| {
                Self::preview_frame(card, logical_card, origin.pointer(), preview, flipped)
            });
            Self::decorate(&dragging, payload.preview(), frame);

            let pointer = NSPoint::new(origin.pointer().x, origin.pointer().y);
            let event = self.drag_event(&view, pointer)?;

            let array = NSArray::from_retained_slice(&[dragging]);
            let protocol: &ProtocolObject<dyn NSDraggingSource> =
                ProtocolObject::from_ref(&*source);
            let native = view.beginDraggingSessionWithItems_event_source(&array, &event, protocol);
            native.setAnimatesToStartingPositionsOnCancelOrFail(true);
            Ok(())
        })();

        if let Err(err) = started {
            // Nothing is in flight, so the file goes now rather than waiting for
            // an outcome that will never arrive.
            retire(&source);
            retire_provider(&provider);
            session.finish(DragOutcome::Failed(err.to_string()));
            return Err(err);
        }

        tracing::info!(
            file = %payload.file().file_name(),
            bytes = file_bytes.len(),
            image = !png.is_empty(),
            "drag: session began"
        );
        Ok(session)
    }
}

#[cfg(test)]
mod geometry_tests {
    use super::*;
    use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};

    #[test]
    fn a_smaller_preview_preserves_the_grab_point_in_a_flipped_view() {
        let logical = LogicalRect::new(
            LogicalPoint::new(10.0, 20.0),
            LogicalSize::new(210.0, 150.0),
        );
        let card = AppKitRect::new(10.0, 20.0, 210.0, 150.0);
        let preview =
            DragPreview::from_png(vec![1], LogicalSize::new(168.0, 96.0)).expect("preview");
        let frame = MacDragSource::preview_frame(
            card,
            logical,
            LogicalPoint::new(115.0, 95.0),
            &preview,
            true,
        );

        assert!((frame.x + 84.0 - 115.0).abs() < f64::EPSILON);
        assert!((frame.y + 48.0 - 95.0).abs() < f64::EPSILON);
        assert_eq!((frame.width, frame.height), (168.0, 96.0));
    }

    #[test]
    fn an_unflipped_preview_keeps_the_pointer_over_the_same_content() {
        let logical = LogicalRect::new(
            LogicalPoint::new(10.0, 20.0),
            LogicalSize::new(210.0, 150.0),
        );
        let card = AppKitRect::new(10.0, 673.0, 210.0, 150.0);
        let preview =
            DragPreview::from_png(vec![1], LogicalSize::new(168.0, 96.0)).expect("preview");
        let frame = MacDragSource::preview_frame(
            card,
            logical,
            LogicalPoint::new(115.0, 95.0),
            &preview,
            false,
        );
        let pointer_y = card.y + card.height - 75.0;

        assert!((frame.x + 84.0 - 115.0).abs() < f64::EPSILON);
        assert!((frame.y + (96.0 - 48.0) - pointer_y).abs() < f64::EPSILON);
    }

    #[test]
    fn a_card_matched_preview_starts_on_the_exact_source_bounds() {
        let logical = LogicalRect::new(
            LogicalPoint::new(10.0, 20.0),
            LogicalSize::new(210.0, 150.0),
        );
        let preview =
            DragPreview::from_png(vec![1], LogicalSize::new(210.0, 150.0)).expect("preview");
        for (card, pointer, flipped) in [
            (
                AppKitRect::new(10.0, 20.0, 210.0, 150.0),
                LogicalPoint::new(118.0, 95.0),
                true,
            ),
            (
                AppKitRect::new(10.0, 673.0, 210.0, 150.0),
                LogicalPoint::new(118.0, 95.0),
                false,
            ),
        ] {
            let frame = MacDragSource::preview_frame(card, logical, pointer, &preview, flipped);
            for (actual, expected) in [
                (frame.x, card.x),
                (frame.y, card.y),
                (frame.width, card.width),
                (frame.height, card.height),
            ] {
                assert!((actual - expected).abs() < 1e-9, "{frame:?} != {card:?}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test-only seams
// ---------------------------------------------------------------------------

/// Test seam: build the pasteboard item a drag would carry, without a drag.
///
/// This exists so the parts that *can* be verified on a machine whose mouse is
/// in use — which flavours are advertised, in which order, that the file URL
/// round-trips to the path that was written, that the image flavours produce
/// real bytes — actually are. Nothing here touches a window, the system
/// pasteboard, or the pointer.
#[doc(hidden)]
pub mod test_support {
    use super::{
        ClassType, DragPayload, NSPasteboardItem, NSString, NSURL, NSView, Result, Retained,
        ScrozzImageProvider, capture_item, hold_provider, retire_provider, sel,
    };
    use crate::drag::artifact::DragArtifact;

    /// One capture's pasteboard item, exactly as [`MacDragSource`](super::MacDragSource)`::begin`
    /// would build it.
    pub struct ItemHarness {
        item: Retained<NSPasteboardItem>,
        provider: Retained<ScrozzImageProvider>,
        artifact: DragArtifact,
    }

    impl ItemHarness {
        /// Materialises `payload` under `root` and builds its pasteboard item.
        ///
        /// # Errors
        ///
        /// Propagates whatever the byte producer, the filesystem or AppKit
        /// returned.
        pub fn new(payload: &DragPayload, root: &std::path::Path) -> Result<Self> {
            // Mirrors `begin` exactly, including the split between the bytes
            // written to disk and the bytes offered as an image. A harness that
            // shortcut that split would report flavours the real drag does not
            // advertise.
            let (artifact, bytes) = payload.materialise(root)?;
            let file_bytes = std::sync::Arc::new(bytes);
            let png = payload
                .image_png(&file_bytes)?
                .unwrap_or_else(|| std::sync::Arc::new(Vec::new()));
            let provider = ScrozzImageProvider::new(std::sync::Arc::clone(&png));
            hold_provider(&provider);
            let item = capture_item(artifact.path(), &png, &provider)?;
            Ok(Self {
                item,
                provider,
                artifact,
            })
        }

        /// The flavours the item advertises, in the order AppKit will offer
        /// them.
        #[must_use]
        pub fn types(&self) -> Vec<String> {
            self.item
                .types()
                .iter()
                .map(|ty| ty.to_string())
                .collect::<Vec<_>>()
        }

        /// The path the advertised `public.file-url` resolves back to.
        #[must_use]
        pub fn file_url_path(&self) -> Option<String> {
            let ty = NSString::from_str("public.file-url");
            let value = self.item.stringForType(&ty)?;
            let url = NSURL::URLWithString(&value)?;
            url.path().map(|path| path.to_string())
        }

        /// Reads one flavour, running the lazy provider when the item has no
        /// eager data for it.
        #[must_use]
        pub fn flavour(&self, uti: &str) -> Option<Vec<u8>> {
            let requested = NSString::from_str(uti);
            if let Some(data) = self.item.dataForType(&requested) {
                return Some(data.to_vec());
            }
            self.provider.provide_image(&self.item, &requested);
            self.item.dataForType(&requested).map(|data| data.to_vec())
        }

        /// The artifact backing this item, so a test can assert its lifetime.
        #[must_use]
        pub fn artifact(&self) -> &DragArtifact {
            &self.artifact
        }

        /// The artifact, mutably, so a test can drive its state transitions.
        pub fn artifact_mut(&mut self) -> &mut DragArtifact {
            &mut self.artifact
        }

        /// Releases the provider as `pasteboardFinishedWithDataProvider:` would.
        pub fn finish(&self) {
            retire_provider(&self.provider);
        }
    }

    /// Whether `NSView` implements the drag entry point at all.
    ///
    /// This answers only the part that is checkable without a mouse. Whether a
    /// *non-activating* panel can host a real drop still needs a human; see
    /// `docs/drag-matrix.md`.
    #[must_use]
    pub fn view_can_begin_drags() -> bool {
        NSView::class().responds_to(sel!(beginDraggingSessionWithItems:event:source:))
    }
}
