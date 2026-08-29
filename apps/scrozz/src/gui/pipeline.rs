//! The capture worker: everything that must not happen on the main thread.
//!
//! # Why a thread at all
//!
//! Reading the screen is a synchronous call into ScreenCaptureKit or the
//! portal, encoding a 3456×2234 PNG is tens of milliseconds, and SQLite writes
//! block. The main thread is servicing winit, the tray and the hotkey queue; a
//! capture taken on it stalls all three, and the visible symptom is that the
//! card animating in stutters at exactly the moment the user is watching it.
//!
//! So the main thread does one thing on a hotkey: post a [`Job`]. Everything
//! after that happens here, and comes back as an [`Outcome`] the main thread
//! picks up on its next tick.
//!
//! # What the worker owns
//!
//! The store handle, and the encoded bytes of the cards still on screen. Both
//! deliberately. `SqliteStore` holds a `rusqlite::Connection`, so keeping it on
//! one thread means there is exactly one writer and no lock discipline to get
//! wrong. And keeping the *encoded* bytes rather than the `Frame` is what makes
//! "copy this card" cheap without holding 30 MB of RGBA per card — the PNG of
//! the same capture is a few hundred kilobytes, and `scrozz-export` decodes it
//! back when the clipboard asks.
//!
//! # Why the bytes are shared and not private
//!
//! Copy and Save are asynchronous: the user presses a button, the worker does
//! the work, an [`Outcome`] arrives a frame or two later, and nobody minds.
//! A **drag** is not. AppKit will only start a dragging session while the mouse
//! button is still held, so at the instant the gesture commits the main thread
//! must already have the full-resolution PNG in hand — a round trip through
//! this channel would arrive after the button came up, which is the difference
//! between a drop and a card that animates away into nothing.
//!
//! So the encoded bytes live in a [`CaptureVault`]: the worker fills it, the
//! main thread reads it, and both refer to the same `Arc<Vec<u8>>` rather than
//! copying a few hundred kilobytes around. It is the only shared mutable state
//! between the two threads, it is behind one mutex, and no lock is ever held
//! across a capture, an encode or a filesystem call.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    thread::JoinHandle,
    time::SystemTime,
};

use scrozz_annotate::{Document, DocumentData};
use scrozz_core::{
    Capture, CaptureRequest, CaptureTarget, ColorSpace, CursorMode, Error as CoreError, Provenance,
    ScaleFactor, SelectionMode, SelectionOptions,
};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, RgbaImage, SystemClipboard};
use scrozz_store::{CaptureId, History, NewCapture, SqliteStore};
use scrozz_ui::editor::RevisionedFrame;

use crate::{
    fault::{CliError, CliResult},
    gui::{
        action::CaptureKind,
        card::{Card, CardId, THUMBNAIL_MAX_EDGE, Thumbnail},
        selection::CaptureSelector,
    },
    platform,
};

/// Work posted to the capture thread.
#[derive(Debug)]
pub enum Job {
    /// Take a capture and turn it into a card.
    Capture {
        /// What to capture.
        kind: CaptureKind,
        /// The user-action source retained for an exact permission retry.
        origin: &'static str,
        /// The identity the resulting card will carry, allocated up front so the
        /// main thread can correlate the answer with the request.
        card: CardId,
    },
    /// Process pixels from the exact one-shot filter returned by Apple's limited picker.
    #[cfg(target_os = "macos")]
    ApplePickerCapture {
        /// The action whose fallback produced the filter.
        kind: CaptureKind,
        /// The original user-action source.
        origin: &'static str,
        /// The card identity allocated only after Apple returned a selection.
        card: CardId,
        /// Pixels captured from the least-privilege filter.
        capture: scrozz_capture::PickerCapture,
    },
    /// Decode a card's capture so the annotation editor can open on it.
    ///
    /// The decode is the whole reason this is a job: a 6K PNG takes tens of
    /// milliseconds to inflate, which is a visible stutter if it happens
    /// between the click and the window.
    Open(CardId),
    /// Render and encode an editor document ahead of a possible drag.
    PrepareImage {
        /// Which card owns the document.
        card: CardId,
        /// Which opening of that card's editor owns the document.
        generation: u64,
        /// The exact content revision represented by `data`.
        revision: u64,
        /// The editable scene. The worker reconstructs the immutable source
        /// from its own cache.
        data: Box<DocumentData>,
    },
    /// Put a card's capture on the clipboard.
    Copy(CardId),
    /// Put an already-rendered image on the clipboard.
    ///
    /// Used by the editor, which has flattened its own annotations and must not
    /// have the card's unannotated capture substituted for them.
    CopyImage {
        /// Which card the image came from, for the log line.
        card: CardId,
        /// The flattened image and the exact document revision it represents.
        rendered: Box<RevisionedFrame>,
    },
    /// Write an already-rendered image to the configured folder.
    SaveImage {
        /// Which card the image came from, for the log line.
        card: CardId,
        /// The flattened image and the exact document revision it represents.
        rendered: Box<RevisionedFrame>,
    },
    /// Write a card's capture to the configured folder.
    Save(CardId),
    /// Forget a card's cached bytes. The card itself is the surface's business.
    Release(CardId),
    /// Finish, so the thread can be joined.
    Stop,
}

/// What the capture thread produced.
#[derive(Debug)]
pub enum Outcome {
    /// A capture succeeded and is ready to show.
    Ready(Box<Card>),
    /// A card's capture was decoded and the editor can open on it.
    Opened {
        /// Which card.
        card: CardId,
        /// The decoded capture.
        capture: Box<Capture>,
    },
    /// An editor revision is encoded and ready for synchronous drag use.
    Prepared {
        /// Which card owns the rendered revision.
        card: CardId,
        /// Which opening of the editor produced it.
        generation: u64,
        /// The revision now available in the vault.
        revision: u64,
    },
    /// An editor revision could not be prepared.
    PreparationFailed {
        /// Which card owns the document.
        card: CardId,
        /// Which opening of the editor produced it.
        generation: u64,
        /// The revision that failed.
        revision: u64,
        /// Why it failed.
        error: CliError,
    },
    /// A capture failed. The main thread says why and shows nothing.
    Failed {
        /// Which card was expected.
        card: CardId,
        /// Which action may be retried after a permission change.
        kind: CaptureKind,
        /// The original user-action source.
        origin: &'static str,
        /// Why it will not arrive.
        error: CliError,
    },
    /// A card action completed, with a phrase for the log.
    Done {
        /// Which card.
        card: CardId,
        /// What happened, e.g. "copied to the clipboard".
        detail: String,
    },
    /// A card action failed.
    Refused {
        /// Which card.
        card: CardId,
        /// Why.
        error: CliError,
    },
}

/// A handle to the capture thread.
pub struct Pipeline {
    jobs: Sender<Job>,
    outcomes: Receiver<Outcome>,
    worker: Option<JoinHandle<()>>,
    next_card: u64,
    vault: CaptureVault,
}

impl Pipeline {
    /// Starts the worker.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Core`] if the thread could not be spawned. A store
    /// that will not open is *not* an error here: it is a degradation the worker
    /// reports and continues past, because a capture the user can see and copy
    /// is worth more than a capture refused because history was unavailable.
    pub fn start(selector: Arc<dyn CaptureSelector>) -> CliResult<Self> {
        let (jobs, job_rx) = channel();
        let (outcome_tx, outcomes) = channel();
        let vault = CaptureVault::new();
        let worker_vault = vault.clone();

        let worker = std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || Worker::new(outcome_tx, selector, worker_vault).run(&job_rx))
            .map_err(|err| {
                CliError::Core(CoreError::Platform(format!(
                    "could not start the capture worker: {err}"
                )))
            })?;

        Ok(Self {
            jobs,
            outcomes,
            worker: Some(worker),
            next_card: 1,
            vault,
        })
    }

    /// The encoded captures, for callers that need the bytes synchronously.
    ///
    /// The one such caller is a drag: it must write a real file while the mouse
    /// button is still down. Everything else should post a [`Job`] instead.
    #[must_use]
    pub fn captures(&self) -> CaptureVault {
        self.vault.clone()
    }

    /// Allocates the next card identity.
    pub const fn allocate(&mut self) -> CardId {
        let id = CardId(self.next_card);
        self.next_card += 1;
        id
    }

    /// Posts a job. Returns `false` if the worker has gone.
    pub fn post(&self, job: Job) -> bool {
        self.jobs.send(job).is_ok()
    }

    /// Takes one finished piece of work, if there is one. Never blocks.
    pub fn poll(&self) -> Option<Outcome> {
        self.outcomes.try_recv().ok()
    }

    /// Stops the worker and waits for it.
    ///
    /// Called from `Drop`, but exposed so a host can shut down deterministically
    /// rather than at an unspecified point during teardown.
    pub fn stop(&mut self) {
        let _ = self.jobs.send(Job::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the worker remembers about a card it produced.
#[derive(Clone)]
struct Cached {
    /// The untouched card capture. This remains the default after the editor
    /// closes, per D14.
    bytes: CaptureBytes,
    /// A revision prepared only for an active editor's output and drag paths.
    rendered: Option<CaptureBytes>,
    /// Where the pixels came from, kept alongside them.
    ///
    /// The cached PNG cannot carry this, and decision D9 turns on it: a window
    /// capture must refuse beautification wherever it is reconstructed. Losing
    /// it on the way into the editor would silently hand a window capture the
    /// backdrop it is not allowed to have.
    provenance: Provenance,
    /// What was aimed at, for the same reason.
    target: CaptureTarget,
    /// The display scale the pixels were captured at.
    ///
    /// A PNG has no notion of backing scale, so `decode` can only return
    /// `IDENTITY`. Restoring the original matters because every logical
    /// coordinate in the editor is derived from it: reopening a Retina capture
    /// at 1.0 would double every annotation's apparent size and export at the
    /// wrong resolution.
    scale: ScaleFactor,
    /// How the samples should be interpreted.
    ///
    /// `decode` reports `Unknown` for the same reason — the encoder does not
    /// round-trip a colour profile — and a capture that was known to be
    /// Display P3 must not silently become unlabelled on its way to the editor.
    color_space: ColorSpace,
}

/// What is kept for a card that is still on screen.
#[derive(Clone)]
pub struct CaptureBytes {
    /// The editor lifetime these bytes belong to. `None` is the untouched card
    /// capture.
    pub(crate) generation: Option<u64>,
    /// The document revision these bytes represent. An untouched capture is
    /// revision zero.
    pub(crate) revision: u64,
    /// The full-resolution PNG. What a copy, a save or a drag hands over.
    pub full: Arc<Vec<u8>>,
    /// A small PNG of the same capture, for a drag image. `None` if the
    /// thumbnail could not be encoded, which costs a drag its picture but not
    /// its payload.
    pub preview: Option<Arc<Vec<u8>>>,
}

impl CaptureBytes {
    /// Encodes one exact editor render ahead of synchronous drag use.
    pub(crate) fn from_rendered(generation: u64, rendered: &RevisionedFrame) -> CliResult<Self> {
        let full = FrameEncoder::new()
            .encode(rendered.frame(), ImageFormat::Png)
            .map(Arc::new)?;
        let preview = Thumbnail::from_frame(rendered.frame(), THUMBNAIL_MAX_EDGE)
            .ok()
            .as_ref()
            .and_then(preview_png)
            .map(Arc::new);
        Ok(Self {
            generation: Some(generation),
            revision: rendered.revision(),
            full,
            preview,
        })
    }

    /// The document revision encoded in these bytes.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// The editor lifetime these bytes were rendered for.
    #[must_use]
    pub const fn generation(&self) -> Option<u64> {
        self.generation
    }
}

/// The encoded captures of every card still in the pile.
///
/// Cheap to clone: both ends hold the same map. See the module header for why
/// this is shared rather than owned by the worker.
#[derive(Clone, Default)]
pub struct CaptureVault {
    inner: Arc<Mutex<HashMap<CardId, Cached>>>,
}

impl CaptureVault {
    /// An empty vault.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Files a freshly encoded capture.
    fn store(&self, card: CardId, cached: Cached) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(card, cached);
        }
    }

    /// Seeds a vault entry without taking a real screenshot.
    #[cfg(test)]
    pub(crate) fn store_test_capture(&self, card: CardId, capture: &Capture) -> CliResult<()> {
        let full = FrameEncoder::new()
            .encode(&capture.frame, ImageFormat::Png)
            .map(Arc::new)?;
        self.store(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full,
                    preview: None,
                },
                rendered: None,
                provenance: capture.provenance,
                target: capture.target.clone(),
                scale: capture.frame.scale,
                color_space: capture.frame.color_space,
            },
        );
        Ok(())
    }

    /// The bytes for `card`, if it is still in the pile.
    ///
    /// Returns a clone of the handles, not of the bytes, so the lock is held
    /// for a pointer copy and nothing else.
    #[must_use]
    pub fn get(&self, card: CardId) -> Option<CaptureBytes> {
        self.inner
            .lock()
            .ok()?
            .get(&card)
            .map(|cached| cached.bytes.clone())
    }

    /// Gets only bytes that match `revision`.
    ///
    /// The original capture is revision zero. Later revisions come from the
    /// renderer and are kept separately so closing the editor restores the
    /// card's untouched source rather than silently replacing it.
    #[must_use]
    pub fn get_revision(
        &self,
        card: CardId,
        generation: u64,
        revision: u64,
    ) -> Option<CaptureBytes> {
        let map = self.inner.lock().ok()?;
        let cached = map.get(&card)?;
        cached
            .rendered
            .as_ref()
            .filter(|bytes| bytes.generation() == Some(generation) && bytes.revision() == revision)
            .cloned()
    }

    /// Installs a prepared editor revision without replacing the card capture.
    fn store_rendered(&self, card: CardId, bytes: CaptureBytes) -> bool {
        let Ok(mut map) = self.inner.lock() else {
            return false;
        };
        let Some(cached) = map.get_mut(&card) else {
            return false;
        };
        let version = (bytes.generation().unwrap_or(0), bytes.revision());
        let should_replace = cached.rendered.as_ref().is_none_or(|current| {
            (current.generation().unwrap_or(0), current.revision()) <= version
        });
        if should_replace {
            cached.rendered = Some(bytes);
        }
        true
    }

    /// The full cached entry, including metadata needed to reopen the editor.
    fn cached(&self, card: CardId) -> Option<Cached> {
        self.inner.lock().ok()?.get(&card).cloned()
    }

    /// Drops a card's bytes. Called when the card leaves the pile.
    ///
    /// A drag that is still in flight is unaffected: the file it advertised was
    /// already written, and its own bytes are held by the drag session.
    fn forget(&self, card: CardId) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(&card);
        }
    }

    /// How many captures are being held. For tests and diagnostics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map_or(0, |map| map.len())
    }

    /// Whether anything is being held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct Worker {
    outcomes: Sender<Outcome>,
    selector: Arc<dyn CaptureSelector>,
    store: Option<SqliteStore>,
    vault: CaptureVault,
}

struct CaptureLifecycle {
    selector: Arc<dyn CaptureSelector>,
    active: bool,
}

impl CaptureLifecycle {
    fn new(selector: Arc<dyn CaptureSelector>) -> Self {
        Self {
            selector,
            active: true,
        }
    }

    fn finish(&mut self) {
        if self.active {
            self.selector.capture_finished();
            self.active = false;
        }
    }
}

impl Drop for CaptureLifecycle {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Worker {
    fn new(
        outcomes: Sender<Outcome>,
        selector: Arc<dyn CaptureSelector>,
        vault: CaptureVault,
    ) -> Self {
        // Opened once, here, rather than per capture: the schema check and the
        // directory creation are not free, and doing them on the shutter path
        // would put them between the keypress and the card.
        let store = match SqliteStore::open_default() {
            Ok(store) => {
                tracing::debug!(root = %store.layout().root().display(), "history opened");
                Some(store)
            }
            Err(err) => {
                // Degradation, not failure: captures still reach the clipboard
                // and the filesystem, they just are not remembered.
                tracing::warn!("history is unavailable, captures will not be kept: {err}");
                None
            }
        };

        Self {
            outcomes,
            selector,
            store,
            vault,
        }
    }

    fn run(mut self, jobs: &Receiver<Job>) {
        while let Ok(job) = jobs.recv() {
            match job {
                Job::Capture { kind, origin, card } => self.capture(kind, origin, card),
                #[cfg(target_os = "macos")]
                Job::ApplePickerCapture {
                    kind,
                    origin,
                    card,
                    capture,
                } => self.capture_apple_picker(kind, origin, card, capture),
                Job::Open(card) => self.open(card),
                Job::PrepareImage {
                    card,
                    generation,
                    revision,
                    data,
                } => self.prepare_image(card, generation, revision, *data),
                Job::Copy(card) => self.copy(card),
                Job::CopyImage { card, rendered } => self.copy_image(card, &rendered),
                Job::SaveImage { card, rendered } => self.save_image(card, &rendered),
                Job::Save(card) => self.save(card),
                Job::Release(card) => self.vault.forget(card),
                Job::Stop => break,
            }
        }
        tracing::debug!("capture worker stopped");
    }

    fn capture(&mut self, kind: CaptureKind, origin: &'static str, card: CardId) {
        let mut lifecycle = CaptureLifecycle::new(Arc::clone(&self.selector));
        let result = self.take(kind, card, &mut lifecycle);
        match result {
            Ok(built) => {
                let _ = self.outcomes.send(Outcome::Ready(Box::new(built)));
            }
            Err(error) if error.is_cancellation() => {
                tracing::debug!(%card, "capture selection cancelled");
            }
            Err(error) => {
                tracing::warn!(%card, "capture failed: {error}");
                let _ = self.outcomes.send(Outcome::Failed {
                    card,
                    kind,
                    origin,
                    error,
                });
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn capture_apple_picker(
        &mut self,
        kind: CaptureKind,
        origin: &'static str,
        card: CardId,
        picker_capture: scrozz_capture::PickerCapture,
    ) {
        let result = picker_capture
            .into_capture()
            .map_err(CliError::Core)
            .and_then(|capture| self.finish_capture(kind, card, capture));

        match result {
            Ok(card) => {
                let _ = self.outcomes.send(Outcome::Ready(Box::new(card)));
            }
            Err(error) if error.is_cancellation() => {
                tracing::debug!(%card, "Apple picker capture cancelled");
            }
            Err(error) => {
                tracing::warn!(%card, "Apple picker capture failed: {error}");
                let _ = self.outcomes.send(Outcome::Failed {
                    card,
                    kind,
                    origin,
                    error,
                });
            }
        }
    }

    fn take(
        &mut self,
        kind: CaptureKind,
        card: CardId,
        lifecycle: &mut CaptureLifecycle,
    ) -> CliResult<Card> {
        // Through `platform`, not `scrozz_capture` directly, so the
        // SCROZZ_UNSTABLE_BACKENDS guard still applies to the GUI path.
        let backend = platform::capture_backend()?;
        let mut selection_outcome = None;
        let target = match kind {
            // The one capture with nothing to choose, so it needs nothing but a
            // backend. That is why it is the default hotkey.
            CaptureKind::Fullscreen => {
                let target = CaptureTarget::Display(backend.active_display()?.id);
                self.selector
                    .begin_capture(backend.excludes_current_process(&target))?;
                target
            }
            CaptureKind::AllDisplays => {
                let target = CaptureTarget::AllDisplays;
                self.selector
                    .begin_capture(backend.excludes_current_process(&target))?;
                target
            }
            CaptureKind::AllInOne | CaptureKind::Region | CaptureKind::Window => {
                let options = Self::options_for(kind);
                let capabilities = self.selector.capabilities();
                if !capabilities.supports(options.mode) {
                    return Err(CliError::Core(CoreError::Unsupported {
                        what: format!("choosing a {} on screen", kind.label()),
                        why: format!(
                            "the {} selector does not support {} mode",
                            self.selector.name(),
                            options.mode.label()
                        ),
                    }));
                }
                let outcome = self.selector.select_for_capture(
                    &capabilities.honour(&options),
                    CursorMode::Hidden,
                    false,
                )?;
                selection_outcome = Some(outcome.clone());
                outcome.target
            }
        };

        let include_window_shadow = target.is_window();
        let request = CaptureRequest {
            target,
            cursor: CursorMode::Hidden,
            include_window_shadow,
        };

        let capture = match self.selector.take_frozen_capture(&request) {
            Some(capture) => capture,
            None => crate::gui::selection::capture_selected(
                backend.as_ref(),
                &request,
                selection_outcome.as_ref(),
            )?,
        };
        lifecycle.finish();
        if let Some(outcome) = selection_outcome.as_ref()
            && outcome.mode == SelectionMode::Region
            && let Some(rect) = outcome.rect
            && let Err(error) = remember_region(backend.as_ref(), rect, outcome)
        {
            tracing::warn!("the capture succeeded but its region could not be remembered: {error}");
        }
        self.finish_capture(kind, card, capture)
    }

    fn finish_capture(
        &mut self,
        kind: CaptureKind,
        card: CardId,
        capture: Capture,
    ) -> CliResult<Card> {
        let bytes = FrameEncoder::new().encode(&capture.frame, ImageFormat::Png)?;
        let thumbnail = Thumbnail::from_frame(&capture.frame, THUMBNAIL_MAX_EDGE).ok();
        let capture_id = self.remember(&capture);

        // Encoded here, on the worker, because the only caller is a drag and a
        // drag has no time to spare once it has started.
        let preview = thumbnail.as_ref().and_then(preview_png).map(Arc::new);
        self.vault.store(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(bytes),
                    preview,
                },
                rendered: None,
                provenance: capture.provenance,
                target: capture.target.clone(),
                scale: capture.frame.scale,
                color_space: capture.frame.color_space,
            },
        );

        Ok(Card {
            id: card,
            capture_id,
            kind,
            provenance: capture.provenance,
            source_width: capture.frame.width(),
            source_height: capture.frame.height(),
            scale: capture.frame.scale.get(),
            thumbnail,
            // History persistence is internal and does not count as an export.
            // A visible file is created only when the user presses Save; writing
            // one here made Save create a duplicate a few seconds later.
            written: Vec::new(),
            taken_at: SystemTime::now(),
        })
    }

    fn options_for(kind: CaptureKind) -> SelectionOptions {
        match kind {
            CaptureKind::AllInOne => SelectionOptions::default(),
            CaptureKind::Region => SelectionOptions {
                hud: false,
                ..SelectionOptions::for_mode(SelectionMode::Region)
            },
            CaptureKind::Window => SelectionOptions {
                hud: false,
                ..SelectionOptions::for_mode(SelectionMode::Window)
            },
            CaptureKind::Fullscreen | CaptureKind::AllDisplays => {
                unreachable!("fixed targets never ask for selector options")
            }
        }
    }

    fn prepare_image(&mut self, card: CardId, generation: u64, revision: u64, data: DocumentData) {
        let result = self
            .capture_from_cache(card, "prepare for drag")
            .and_then(|capture| Document::from_data(capture, data).map_err(CliError::Core))
            .and_then(|document| {
                RevisionedFrame::from_document(&document, revision).map_err(CliError::Core)
            })
            .and_then(|rendered| CaptureBytes::from_rendered(generation, &rendered));
        let outcome = match result {
            Ok(bytes) => {
                if self.vault.store_rendered(card, bytes) {
                    Outcome::Prepared {
                        card,
                        generation,
                        revision,
                    }
                } else {
                    Outcome::PreparationFailed {
                        card,
                        generation,
                        revision,
                        error: CliError::usage(format!(
                            "{card} disappeared before revision {revision} was prepared"
                        )),
                    }
                }
            }
            Err(error) => Outcome::PreparationFailed {
                card,
                generation,
                revision,
                error,
            },
        };
        let _ = self.outcomes.send(outcome);
    }

    /// Persists a capture, or explains in the log why it was not.
    fn remember(&mut self, capture: &Capture) -> Option<CaptureId> {
        let store = self.store.as_mut()?;
        let document = Document::new(capture.clone());
        match store.insert(NewCapture::new(&document)) {
            Ok(id) => Some(id),
            Err(err) => {
                tracing::warn!("could not add the capture to history: {err}");
                None
            }
        }
    }

    /// Decodes a card's capture and hands it to the editor.
    ///
    /// The provenance travels with it, because decision D9 makes beautification
    /// illegal for window captures and the editor must be told which kind it
    /// holds rather than guess.
    fn open(&mut self, card: CardId) {
        let decoded = self.capture_from_cache(card, "open");
        match decoded {
            Ok(capture) => {
                let _ = self.outcomes.send(Outcome::Opened {
                    card,
                    capture: Box::new(capture),
                });
            }
            Err(error) => {
                let _ = self.outcomes.send(Outcome::Refused { card, error });
            }
        }
    }

    fn copy_image(&mut self, card: CardId, rendered: &RevisionedFrame) {
        tracing::debug!(
            %card,
            revision = rendered.revision(),
            "copying a rendered document revision"
        );
        let result = SystemClipboard::new()
            .write_image_reporting(rendered.frame())
            .map(|_report| "copied the annotated image".to_owned())
            .map_err(CliError::from);
        self.answer(card, result);
    }

    fn save_image(&mut self, card: CardId, rendered: &RevisionedFrame) {
        tracing::debug!(
            %card,
            revision = rendered.revision(),
            "saving a rendered document revision"
        );
        let result = FrameEncoder::new()
            .encode(rendered.frame(), ImageFormat::Png)
            .map_err(CliError::from)
            .and_then(|bytes| {
                let path = crate::output::export_default(&bytes)?;
                Ok(format!("saved the annotated image to {}", path.display()))
            });
        self.answer(card, result);
    }

    fn copy(&mut self, card: CardId) {
        // The round trip through PNG is deliberate — see the module docs — and
        // is also what will make "copy" work for a card whose capture arrived
        // over IPC, where the worker never held a `Frame` at all.
        let result = self
            .cached(card, "copy")
            .and_then(|cached| Ok(scrozz_export::decode(&cached.full)?))
            .and_then(|frame| {
                SystemClipboard::new().write_image_reporting(&frame)?;
                Ok("copied to the clipboard".to_owned())
            });
        self.answer(card, result);
    }

    fn save(&mut self, card: CardId) {
        let result = self.cached(card, "save").and_then(|cached| {
            let path = crate::output::export_default(&cached.full)?;
            Ok(format!("saved to {}", path.display()))
        });
        self.answer(card, result);
    }

    fn cached(&self, card: CardId, verb: &str) -> CliResult<CaptureBytes> {
        self.vault
            .get(card)
            .ok_or_else(|| CliError::usage(format!("{card} has no capture to {verb}")))
    }

    fn cached_entry(&self, card: CardId, verb: &str) -> CliResult<Cached> {
        self.vault
            .cached(card)
            .ok_or_else(|| CliError::usage(format!("{card} has no capture to {verb}")))
    }

    fn capture_from_cache(&self, card: CardId, verb: &str) -> CliResult<Capture> {
        let cached = self.cached_entry(card, verb)?;
        let mut frame = scrozz_export::decode(&cached.bytes.full)?;
        // PNG carries neither backing scale nor colour space. Restore both from
        // capture metadata before editor geometry or rendering sees it.
        frame.scale = cached.scale;
        frame.color_space = cached.color_space;
        Ok(Capture {
            frame,
            provenance: cached.provenance,
            target: cached.target,
        })
    }

    fn answer(&self, card: CardId, result: CliResult<String>) {
        let message = match result {
            Ok(detail) => Outcome::Done { card, detail },
            Err(error) => Outcome::Refused { card, error },
        };
        let _ = self.outcomes.send(message);
    }
}

fn remember_region(
    backend: &dyn scrozz_core::CaptureBackend,
    rect: scrozz_core::LogicalRect,
    outcome: &scrozz_core::SelectionOutcome,
) -> scrozz_core::Result<()> {
    let displays = backend.displays()?;
    let display = outcome
        .display
        .as_ref()
        .and_then(|id| displays.iter().find(|display| display.id == *id));
    crate::selection_store::RememberedRegionStore::default_location()?
        .save(crate::selection_store::RememberedRegion::new(rect, display))
}

/// Encodes a card's thumbnail as a PNG, for use as a drag image.
///
/// Small on purpose: this is the picture that follows the cursor, not the
/// payload. A failure is logged and swallowed — a drag without a picture is a
/// cosmetic loss, a drag refused because a picture would not encode is not.
fn preview_png(thumbnail: &Thumbnail) -> Option<Vec<u8>> {
    let image = RgbaImage {
        width: thumbnail.width(),
        height: thumbnail.height(),
        data: thumbnail.pixels().to_vec(),
    };
    match FrameEncoder::new().encode_rgba(&image, ColorSpace::Srgb, ImageFormat::Png) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            tracing::debug!("the drag preview could not be encoded: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{
        Error, Frame, RegionSelector, Result as CoreResult, SelectionCapabilities, SelectionOutcome,
    };

    struct RefusingSelector;

    impl RegionSelector for RefusingSelector {
        fn name(&self) -> &'static str {
            "test-refusal"
        }

        fn capabilities(&self) -> SelectionCapabilities {
            SelectionCapabilities::NONE
        }

        fn select(&self, _options: &SelectionOptions) -> CoreResult<SelectionOutcome> {
            Err(Error::Unsupported {
                what: "selection in a pipeline unit test".to_owned(),
                why: "the unit test did not provide a selection".to_owned(),
            })
        }
    }

    impl CaptureSelector for RefusingSelector {}

    fn start_pipeline() -> Pipeline {
        Pipeline::start(Arc::new(RefusingSelector)).expect("the worker should start")
    }

    /// Builds a worker whose cache is seeded by hand.
    ///
    /// The real cache is filled by an actual capture, which a unit test has no
    /// way to perform, so the reconstruction path is exercised directly.
    fn worker_holding(card: CardId, cached: Cached) -> (Worker, Receiver<Outcome>) {
        let (outcomes, inbox) = std::sync::mpsc::channel();
        let vault = CaptureVault::new();
        vault.store(card, cached);
        let worker = Worker {
            outcomes,
            selector: Arc::new(RefusingSelector),
            store: None,
            vault,
        };
        (worker, inbox)
    }

    fn one_pixel_png() -> Vec<u8> {
        let frame = Frame {
            data: vec![255, 255, 255, 255],
            size: scrozz_core::PhysicalSize::new(1.0, 1.0),
            stride: 4,
            format: scrozz_core::PixelFormat::Rgba8,
            color_space: scrozz_core::ColorSpace::Srgb,
            scale: scrozz_core::ScaleFactor::new(1.0),
        };
        FrameEncoder::new()
            .encode(&frame, ImageFormat::Png)
            .expect("a one pixel frame should encode")
    }

    fn two_by_two_png() -> Vec<u8> {
        let frame = Frame {
            data: vec![255; 16],
            size: scrozz_core::PhysicalSize::new(2.0, 2.0),
            stride: 8,
            format: scrozz_core::PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::new(2.0),
        };
        FrameEncoder::new()
            .encode(&frame, ImageFormat::Png)
            .expect("a two by two frame should encode")
    }

    #[test]
    fn opening_a_capture_restores_the_scale_the_png_could_not_carry() {
        // A PNG has no backing-scale field, so `decode` can only return
        // identity. If the cache did not remember the real one, reopening a
        // Retina capture would halve its logical size: every annotation would
        // land at twice its intended scale and the export would come out at
        // the wrong resolution. This is why the metadata rides alongside the
        // bytes rather than being re-derived from them.
        let card = CardId(1);
        let (mut worker, inbox) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(one_pixel_png()),
                    preview: None,
                },
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::AllDisplays,
                scale: ScaleFactor::new(2.0),
                color_space: ColorSpace::DisplayP3,
            },
        );
        worker.open(card);

        let Some(Outcome::Opened { capture, .. }) = inbox.try_iter().next() else {
            panic!("opening a cached card should produce a capture");
        };
        assert!(
            (capture.frame.scale.get() - 2.0).abs() < f64::EPSILON,
            "the capture's own scale should survive the cache, not decode's identity"
        );
        assert_eq!(
            capture.frame.color_space,
            ColorSpace::DisplayP3,
            "a wide-gamut capture must not become unlabelled on the way to the editor"
        );
    }

    #[test]
    fn a_restored_scale_gives_the_editor_the_right_logical_size() {
        // The consequence, stated as the thing that actually matters: logical
        // geometry is physical size divided by scale, and the editor lays every
        // annotation out in logical coordinates.
        let card = CardId(1);
        let (mut worker, inbox) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(two_by_two_png()),
                    preview: None,
                },
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::AllDisplays,
                scale: ScaleFactor::new(2.0),
                color_space: ColorSpace::Srgb,
            },
        );
        worker.open(card);

        let Some(Outcome::Opened { capture, .. }) = inbox.try_iter().next() else {
            panic!("opening a cached card should produce a capture");
        };
        let logical = f64::from(capture.frame.width()) / capture.frame.scale.get();
        assert!(
            (logical - 1.0).abs() < f64::EPSILON,
            "a 2x2 physical capture at 2.0 scale is 1x1 logical, not 2x2"
        );
    }

    #[test]
    fn an_unknown_colour_space_is_preserved_as_unknown() {
        // The cache restores what it was told, including the absence of a
        // profile. It must not upgrade `Unknown` to `Srgb` on the way through.
        let card = CardId(1);
        let (mut worker, inbox) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(one_pixel_png()),
                    preview: None,
                },
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::AllDisplays,
                scale: ScaleFactor::IDENTITY,
                color_space: ColorSpace::Unknown,
            },
        );
        worker.open(card);

        let Some(Outcome::Opened { capture, .. }) = inbox.try_iter().next() else {
            panic!("opening a cached card should produce a capture");
        };
        assert_eq!(capture.frame.color_space, ColorSpace::Unknown);
    }

    #[test]
    fn opening_a_window_capture_keeps_it_a_window_capture() {
        // D9: a window capture may not be beautified. The editor rebuilds its
        // capture from a cached PNG, which carries no provenance of its own, so
        // if the cache forgot it the editor would hand a window capture the
        // backdrop it is forbidden. This is that guarantee, not a detail.
        let card = CardId(1);
        let (mut worker, inbox) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(one_pixel_png()),
                    preview: None,
                },
                rendered: None,
                provenance: Provenance::Window,
                target: CaptureTarget::Window(scrozz_core::WindowId("test-window".to_owned())),
                scale: ScaleFactor::IDENTITY,
                color_space: ColorSpace::Srgb,
            },
        );
        worker.open(card);

        let Some(Outcome::Opened { capture, .. }) = inbox.try_iter().next() else {
            panic!("opening a cached card should produce a capture");
        };
        assert_eq!(capture.provenance, Provenance::Window);
        assert!(
            !scrozz_annotate::Document::new(*capture).may_beautify(),
            "a window capture must still refuse beautification after a round trip"
        );
    }

    #[test]
    fn opening_a_region_capture_still_allows_beautification() {
        let card = CardId(1);
        let bounds = scrozz_core::LogicalRect::new(
            scrozz_core::LogicalPoint::new(0.0, 0.0),
            scrozz_core::LogicalSize::new(1.0, 1.0),
        );
        let (mut worker, inbox) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(one_pixel_png()),
                    preview: None,
                },
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::Region(bounds),
                scale: ScaleFactor::IDENTITY,
                color_space: ColorSpace::Srgb,
            },
        );
        worker.open(card);

        let Some(Outcome::Opened { capture, .. }) = inbox.try_iter().next() else {
            panic!("opening a cached card should produce a capture");
        };
        assert_eq!(capture.provenance, Provenance::Region);
        assert!(scrozz_annotate::Document::new(*capture).may_beautify());
    }

    #[test]
    fn opening_a_card_that_was_never_captured_is_refused() {
        let (mut worker, inbox) = worker_holding(
            CardId(1),
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(one_pixel_png()),
                    preview: None,
                },
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::AllDisplays,
                scale: ScaleFactor::IDENTITY,
                color_space: ColorSpace::Srgb,
            },
        );
        worker.open(CardId(9));
        assert!(matches!(
            inbox.try_iter().next(),
            Some(Outcome::Refused { card, .. }) if card == CardId(9)
        ));
    }

    #[test]
    fn a_pipeline_hands_out_distinct_card_identities() {
        let mut pipeline = start_pipeline();
        let first = pipeline.allocate();
        let second = pipeline.allocate();
        assert_ne!(first, second);
        assert_eq!(first, CardId(1));
    }

    #[test]
    fn polling_an_idle_pipeline_does_not_block() {
        let pipeline = start_pipeline();
        assert!(pipeline.poll().is_none());
    }

    #[test]
    fn a_pipeline_stops_cleanly_and_twice_is_harmless() {
        // Drop also stops it, so the second call must be a no-op rather than a
        // join on an already-joined handle.
        let mut pipeline = start_pipeline();
        pipeline.stop();
        pipeline.stop();
    }

    #[test]
    fn copying_a_card_that_was_never_captured_is_refused_not_ignored() {
        let pipeline = start_pipeline();
        assert!(pipeline.post(Job::Copy(CardId(404))));

        match wait_for(&pipeline) {
            Some(Outcome::Refused { card, error }) => {
                assert_eq!(card, CardId(404));
                assert!(error.to_string().contains("404"), "{error}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn saving_a_card_that_was_never_captured_is_refused_too() {
        let pipeline = start_pipeline();
        assert!(pipeline.post(Job::Save(CardId(7))));

        match wait_for(&pipeline) {
            Some(Outcome::Refused { card, .. }) => assert_eq!(card, CardId(7)),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn releasing_an_unknown_card_is_harmless() {
        let pipeline = start_pipeline();
        assert!(pipeline.post(Job::Release(CardId(1))));
        assert!(pipeline.post(Job::Release(CardId(1))));
    }

    /// Waits briefly for the worker, so the test does not depend on scheduling.
    fn wait_for(pipeline: &Pipeline) -> Option<Outcome> {
        for _ in 0..200 {
            if let Some(outcome) = pipeline.poll() {
                return Some(outcome);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }
}
