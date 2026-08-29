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
    Capture, CaptureRequest, CaptureTarget, ColorSpace, CursorMode, Error as CoreError, PinState,
    Provenance, ScaleFactor, SelectionMode, SelectionOptions,
};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, RgbaImage};
use scrozz_store::{CaptureId, DocumentState, History, NewCapture, Page, SearchQuery, SqliteStore};
use scrozz_ui::editor::RevisionedFrame;

use crate::{
    after_capture::{
        ActionEffect, ActionExecutor, AfterCaptureAction, AfterCaptureSettings, ExecutionReport,
        FinalizedScreenshot, MediaKind, orchestrate,
    },
    fault::{CliError, CliResult},
    gui::{
        action::{CaptureKind, CaptureOrigin},
        card::{Card, CardId, PIN_TEXTURE_MAX_EDGE, PinnedCapture, THUMBNAIL_MAX_EDGE, Thumbnail},
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
        /// Where the request entered the app.
        origin: CaptureOrigin,
        /// The identity the resulting card will carry, allocated up front so the
        /// main thread can correlate the answer with the request.
        card: CardId,
        /// The persisted GUI policy snapshotted when this capture was requested.
        policy: AfterCaptureSettings,
    },
    /// Process pixels from the exact one-shot filter returned by Apple's limited picker.
    #[cfg(target_os = "macos")]
    ApplePickerCapture {
        /// The action whose fallback produced the filter.
        kind: CaptureKind,
        /// The original user-action source.
        origin: CaptureOrigin,
        /// The card identity allocated only after Apple returned a selection.
        card: CardId,
        /// Pixels captured from the least-privilege filter.
        capture: scrozz_capture::PickerCapture,
        /// The persisted GUI policy snapshotted when this capture was requested.
        policy: AfterCaptureSettings,
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
    /// Persist a new pin using the capture associated with a live card.
    PinCard {
        /// Live card identity.
        card: CardId,
        /// Durable capture identity supplied by the overlay request.
        capture: CaptureId,
        /// Initial durable pin state.
        state: PinState,
    },
    /// Persist a changed pin or clear a closed one.
    SetPin {
        /// Durable history identity.
        capture: CaptureId,
        /// `None` means the pin closed.
        state: Option<PinState>,
    },
    /// Clear a pin after every previously queued worker write, then acknowledge it.
    TerminalUnpin {
        /// Durable history identity.
        capture: CaptureId,
        /// Completion is reported directly so an IPC reply cannot race persistence.
        reply: Sender<CliResult<()>>,
    },
    /// Unlock every persisted pin after all previously queued worker writes.
    UnlockPins {
        /// Completion is reported directly to the external escape route.
        reply: Sender<CliResult<u64>>,
    },
    #[doc(hidden)]
    FlushPins,
    /// Finish, so the thread can be joined.
    Stop,
}

#[derive(Default)]
struct PendingPinUpdates {
    states: Mutex<HashMap<CaptureId, Option<PinState>>>,
}

impl PendingPinUpdates {
    /// Keeps only the newest state per capture and reports whether the worker
    /// needs a wake-up.
    fn queue(&self, capture: CaptureId, state: Option<PinState>) -> bool {
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let needs_wakeup = states.is_empty();
        states.insert(capture, state);
        needs_wakeup
    }

    fn take(&self) -> HashMap<CaptureId, Option<PinState>> {
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *states)
    }

    fn cancel(&self, capture: &CaptureId) {
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(capture);
    }
}

/// What the capture thread produced.
#[derive(Debug)]
pub enum Outcome {
    /// A capture succeeded and is ready to show.
    Ready(Box<ReadyCapture>),
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
        origin: CaptureOrigin,
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
    /// A persisted pin is ready to restore or refresh on screen.
    PinReady(Box<PinnedCapture>),
    /// Higher-resolution pixels for a newly created pin.
    PinTextureReady {
        /// Durable history identity.
        capture: CaptureId,
        /// Pixels only; geometry and presentation remain UI-owned.
        texture: Thumbnail,
    },
    /// The first durable pin write failed, so the visible viewport must roll back.
    PinCreationFailed {
        /// Durable pin identity to remove from the overlay.
        capture: CaptureId,
        /// Explicit storage or identity error.
        error: CliError,
    },
    /// Persisting a pin update failed.
    PinPersistenceFailed {
        /// Capture whose sidecar could not be updated.
        capture: CaptureId,
        /// Explicit storage error.
        error: CliError,
    },
}

/// A finalized card plus every isolated ambient action result.
#[derive(Debug)]
pub struct ReadyCapture {
    /// The surface model built from the immutable artifact.
    pub card: Card,
    /// Enabled actions in deterministic execution order.
    pub actions: ExecutionReport,
}

/// A handle to the capture thread.
pub struct Pipeline {
    jobs: Sender<Job>,
    pending_pin_updates: Arc<PendingPinUpdates>,
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
        Self::start_with_history(selector, true)
    }

    /// Starts the worker with persistent history explicitly enabled or disabled.
    ///
    /// Sealed/headless tests disable it so validation cannot create or migrate a
    /// developer's real profile.
    pub fn start_with_history(
        selector: Arc<dyn CaptureSelector>,
        history_enabled: bool,
    ) -> CliResult<Self> {
        let (jobs, job_rx) = channel();
        let (outcome_tx, outcomes) = channel();
        let vault = CaptureVault::new();
        let worker_vault = vault.clone();
        let pending_pin_updates = Arc::new(PendingPinUpdates::default());
        let worker_pin_updates = Arc::clone(&pending_pin_updates);

        let worker = std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || {
                Worker::new(outcome_tx, selector, worker_vault, history_enabled)
                    .run(&job_rx, &worker_pin_updates);
            })
            .map_err(|err| {
                CliError::Core(CoreError::Platform(format!(
                    "could not start the capture worker: {err}"
                )))
            })?;

        Ok(Self {
            jobs,
            pending_pin_updates,
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
        if let Job::SetPin { capture, state } = job {
            if state.is_none() {
                // Closing is terminal for all geometry already queued by that
                // viewport. If the worker took an older state concurrently,
                // this direct job follows it in the channel and clears it.
                self.pending_pin_updates.cancel(&capture);
                return self.jobs.send(Job::SetPin { capture, state }).is_ok();
            }
            if !self.pending_pin_updates.queue(capture, state) {
                return true;
            }
            if self.jobs.send(Job::FlushPins).is_ok() {
                return true;
            }
            self.pending_pin_updates.take();
            return false;
        }
        self.jobs.send(job).is_ok()
    }

    /// Clears a pin after all older worker jobs and waits for the durable result.
    pub fn terminal_unpin(&self, capture: CaptureId) -> CliResult<()> {
        self.pending_pin_updates.cancel(&capture);
        let (reply, result) = channel();
        self.jobs
            .send(Job::TerminalUnpin { capture, reply })
            .map_err(|_| {
                CliError::Core(CoreError::Platform(
                    "the capture worker stopped before it could persist the unpin".into(),
                ))
            })?;
        result.recv().map_err(|_| {
            CliError::Core(CoreError::Platform(
                "the capture worker stopped without acknowledging the unpin".into(),
            ))
        })?
    }

    /// Unlocks all pins after all older worker jobs and waits for persistence.
    pub fn unlock_pins(&self) -> CliResult<u64> {
        let (reply, result) = channel();
        self.jobs.send(Job::UnlockPins { reply }).map_err(|_| {
            CliError::Core(CoreError::Platform(
                "the capture worker stopped before it could unlock pins".into(),
            ))
        })?;
        result.recv().map_err(|_| {
            CliError::Core(CoreError::Platform(
                "the capture worker stopped without acknowledging the unlock".into(),
            ))
        })?
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
    /// Durable history identity used to validate a card-to-pin transition.
    capture_id: Option<CaptureId>,
}

struct ScreenshotExecutor<'a> {
    policy: &'a AfterCaptureSettings,
}

impl ActionExecutor<FinalizedScreenshot<'_>> for ScreenshotExecutor<'_> {
    fn execute(
        &mut self,
        action: AfterCaptureAction,
        artifact: &FinalizedScreenshot<'_>,
    ) -> std::result::Result<ActionEffect, String> {
        match action {
            AfterCaptureAction::CopyToClipboard => {
                scrozz_shell::write_capture_to_clipboard(artifact.frame, artifact.png)
                    .map(|_| ActionEffect::Completed)
                    .map_err(|error| error.to_string())
            }
            AfterCaptureAction::SaveAutomatically => {
                crate::output::export_with_settings(artifact.png, self.policy)
                    .map(ActionEffect::Saved)
                    .map_err(|error| error.to_string())
            }
            AfterCaptureAction::UploadAndCopyLink => Err(
                "no cloud upload provider is implemented or configured in this build".to_owned(),
            ),
            AfterCaptureAction::ShowRecentCapturesOverlay => {
                Ok(ActionEffect::ShowRecentCapturesOverlay)
            }
            AfterCaptureAction::OpenEditor => Ok(ActionEffect::OpenEditor),
            AfterCaptureAction::PinToScreen => {
                Err("Pin to Screen is not implemented in this build".to_owned())
            }
        }
    }
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
                capture_id: None,
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
        history_enabled: bool,
    ) -> Self {
        // Opened once, here, rather than per capture: the schema check and the
        // directory creation are not free, and doing them on the shutter path
        // would put them between the keypress and the card.
        let store = if !history_enabled {
            None
        } else {
            match SqliteStore::open_default() {
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
            }
        };

        let mut worker = Self {
            outcomes,
            selector,
            store,
            vault,
        };
        worker.restore_existing_pins();
        worker
    }

    fn run(mut self, jobs: &Receiver<Job>, pending_pin_updates: &PendingPinUpdates) {
        while let Ok(job) = jobs.recv() {
            match job {
                Job::Capture {
                    kind,
                    card,
                    origin,
                    policy,
                } => self.capture(kind, card, origin, &policy),
                #[cfg(target_os = "macos")]
                Job::ApplePickerCapture {
                    kind,
                    origin,
                    card,
                    capture,
                    policy,
                } => self.capture_apple_picker(kind, origin, card, capture, &policy),
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
                Job::PinCard {
                    card,
                    capture,
                    state,
                } => self.pin_card(card, &capture, &state),
                Job::SetPin { capture, state } => {
                    self.set_pin(&capture, state.as_ref());
                }
                Job::TerminalUnpin { capture, reply } => {
                    let _ = reply.send(self.persist_pin(&capture, None));
                }
                Job::UnlockPins { reply } => {
                    let result = self
                        .store
                        .as_mut()
                        .ok_or_else(|| {
                            CliError::Core(CoreError::Storage(
                                "history is unavailable, so pinned state cannot be persisted"
                                    .into(),
                            ))
                        })
                        .and_then(|store| store.unlock_screen_pins().map_err(CliError::from));
                    let _ = reply.send(result);
                }
                Job::FlushPins => {
                    for (capture, state) in pending_pin_updates.take() {
                        self.set_pin(&capture, state.as_ref());
                    }
                }
                Job::Stop => break,
            }
        }
        tracing::debug!("capture worker stopped");
    }

    fn capture(
        &mut self,
        kind: CaptureKind,
        card: CardId,
        origin: CaptureOrigin,
        policy: &AfterCaptureSettings,
    ) {
        tracing::debug!(
            %card,
            capture = kind.label(),
            origin = origin.label(),
            "capture job started"
        );
        let mut lifecycle = CaptureLifecycle::new(Arc::clone(&self.selector));
        let result = self.take(kind, card, policy, &mut lifecycle);
        match result {
            Ok(built) => {
                let _ = self.outcomes.send(Outcome::Ready(Box::new(built)));
            }
            Err(error) if error.is_cancellation() => {
                tracing::debug!(%card, origin = origin.label(), "capture selection cancelled");
            }
            Err(error) => {
                tracing::warn!(%card, origin = origin.label(), "capture failed: {error}");
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
        origin: CaptureOrigin,
        card: CardId,
        picker_capture: scrozz_capture::PickerCapture,
        policy: &AfterCaptureSettings,
    ) {
        let result = picker_capture
            .into_capture()
            .map_err(CliError::Core)
            .and_then(|capture| self.finish_capture(kind, card, capture, policy));

        match result {
            Ok(card) => {
                let _ = self.outcomes.send(Outcome::Ready(Box::new(card)));
            }
            Err(error) if error.is_cancellation() => {
                tracing::debug!(%card, "Apple picker capture cancelled");
            }
            Err(error) => {
                tracing::warn!(%card, origin = origin.label(), "Apple picker capture failed: {error}");
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
        policy: &AfterCaptureSettings,
        lifecycle: &mut CaptureLifecycle,
    ) -> CliResult<ReadyCapture> {
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
                // `select_for_capture` owns the timing boundary: its native
                // target snapshot completes before the selector bridge is
                // allowed to mutate any Scrozz surface. Keep selection as the
                // first picker operation on this job.
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
        self.finish_capture(kind, card, capture, policy)
    }

    fn finish_capture(
        &mut self,
        kind: CaptureKind,
        card: CardId,
        capture: Capture,
        policy: &AfterCaptureSettings,
    ) -> CliResult<ReadyCapture> {
        let bytes = Arc::new(FrameEncoder::new().encode(&capture.frame, ImageFormat::Png)?);

        // Finalized bytes and metadata exist before any action runs. Copy is
        // first and happens before thumbnailing or history I/O; durable
        // destinations follow, then host presentation effects are returned.
        let artifact = FinalizedScreenshot {
            frame: &capture.frame,
            png: bytes.as_slice(),
        };
        let mut executor = ScreenshotExecutor { policy };
        let actions = orchestrate(MediaKind::Screenshot, policy, &artifact, &mut executor);

        let thumbnail = Thumbnail::from_frame(&capture.frame, THUMBNAIL_MAX_EDGE).ok();

        // Encoded here, on the worker, because the only caller is a drag and a
        // drag has no time to spare once it has started.
        let preview = thumbnail.as_ref().and_then(preview_png).map(Arc::new);
        let capture_id = self.remember(&capture);
        self.vault.store(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::clone(&bytes),
                    preview,
                },
                rendered: None,
                provenance: capture.provenance,
                target: capture.target.clone(),
                scale: capture.frame.scale,
                color_space: capture.frame.color_space,
                capture_id: capture_id.clone(),
            },
        );

        let written = actions
            .steps
            .iter()
            .filter_map(|step| match &step.outcome {
                crate::after_capture::ActionOutcome::Succeeded(ActionEffect::Saved(path)) => {
                    Some(path.display().to_string())
                }
                crate::after_capture::ActionOutcome::Succeeded(ActionEffect::Uploaded(url)) => {
                    Some(url.clone())
                }
                _ => None,
            })
            .collect();

        Ok(ReadyCapture {
            card: Card {
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
                written,
                taken_at: SystemTime::now(),
            },
            actions,
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
        let result = FrameEncoder::new()
            .encode(rendered.frame(), ImageFormat::Png)
            .and_then(|png| {
                scrozz_shell::write_capture_to_clipboard(rendered.frame(), &png).map(|_| ())
            })
            .map(|()| "copied the annotated image".to_owned())
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
        let result = self.cached_entry(card, "copy").and_then(|cached| {
            let mut frame = scrozz_export::decode(&cached.bytes.full)?;
            frame.scale = cached.scale;
            frame.color_space = cached.color_space;
            scrozz_shell::write_capture_to_clipboard(&frame, &cached.bytes.full)?;
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

    fn pin_card(&mut self, card: CardId, capture: &CaptureId, state: &PinState) {
        let cached_capture = self.vault.cached(card).and_then(|cached| cached.capture_id);
        if cached_capture.as_ref() != Some(capture) {
            let error = CliError::usage(format!(
                "{card} does not own persisted capture {}",
                capture.0
            ));
            let _ = self.outcomes.send(Outcome::PinCreationFailed {
                capture: capture.clone(),
                error,
            });
            self.vault.forget(card);
            return;
        }
        let result = self.persist_pin(capture, Some(state));
        match result {
            Ok(()) => {
                if let Some(texture) = self.load_pin_texture(capture) {
                    let _ = self.outcomes.send(Outcome::PinTextureReady {
                        capture: capture.clone(),
                        texture,
                    });
                }
                self.vault.forget(card);
            }
            Err(error) => {
                let _ = self.outcomes.send(Outcome::PinCreationFailed {
                    capture: capture.clone(),
                    error,
                });
                self.vault.forget(card);
            }
        }
    }

    fn set_pin(&mut self, capture: &CaptureId, state: Option<&PinState>) {
        if let Err(error) = self.persist_pin(capture, state) {
            let _ = self.outcomes.send(Outcome::PinPersistenceFailed {
                capture: capture.clone(),
                error,
            });
        }
    }

    fn persist_pin(&mut self, capture: &CaptureId, state: Option<&PinState>) -> CliResult<()> {
        let Some(store) = self.store.as_mut() else {
            return Err(CliError::Core(CoreError::Storage(
                "history is unavailable, so pinned state cannot be persisted".into(),
            )));
        };
        store.set_screen_pin(capture, state)?;
        Ok(())
    }

    fn restore_existing_pins(&mut self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let query = SearchQuery {
            pinned_only: true,
            page: Page::new(10_000, 0),
            ..SearchQuery::default()
        };
        let records = match store.search(&query) {
            Ok(records) => records,
            Err(err) => {
                tracing::warn!("could not enumerate persisted pinned captures: {err}");
                return;
            }
        };
        let ids: Vec<CaptureId> = records
            .into_iter()
            .filter(|record| record.screen_pin.is_some())
            .map(|record| record.id)
            .collect();
        for id in ids {
            if let Some(pin) = self.load_pin(&id) {
                let _ = self.outcomes.send(Outcome::PinReady(Box::new(pin)));
            }
        }
    }

    fn load_pin(&mut self, id: &CaptureId) -> Option<PinnedCapture> {
        let store = self.store.as_mut()?;
        let record = match store.record(id) {
            Ok(Some(record)) => record,
            Ok(None) => return None,
            Err(err) => {
                tracing::warn!(capture = %id.0, "could not read persisted pin: {err}");
                return None;
            }
        };
        let state = record.screen_pin.clone()?;
        let texture = match store.document(id) {
            Ok(Some(DocumentState::Complete(document))) => {
                Thumbnail::from_frame(&document.source.frame, PIN_TEXTURE_MAX_EDGE).ok()
            }
            Ok(Some(DocumentState::ImageEvicted(_))) | Ok(None) => {
                tracing::warn!(
                    capture = %id.0,
                    "pinned capture source pixels are unavailable; restoring a placeholder"
                );
                None
            }
            Err(err) => {
                tracing::warn!(capture = %id.0, "could not load persisted pin pixels: {err}");
                None
            }
        };
        Some(PinnedCapture {
            id: id.clone(),
            name: record
                .window_title
                .clone()
                .or(record.app_name.clone())
                .unwrap_or_else(|| format!("Capture {}", id.0)),
            provenance: record.provenance,
            source_width: record.frame.size.width.round().max(1.0) as u32,
            source_height: record.frame.size.height.round().max(1.0) as u32,
            scale: record.frame.scale.get(),
            state,
            texture,
        })
    }

    fn load_pin_texture(&mut self, id: &CaptureId) -> Option<Thumbnail> {
        let store = self.store.as_mut()?;
        match store.document(id) {
            Ok(Some(DocumentState::Complete(document))) => {
                Thumbnail::from_frame(&document.source.frame, PIN_TEXTURE_MAX_EDGE).ok()
            }
            Ok(Some(DocumentState::ImageEvicted(_))) | Ok(None) => {
                tracing::warn!(
                    capture = %id.0,
                    "pinned capture source pixels are unavailable"
                );
                None
            }
            Err(err) => {
                tracing::warn!(capture = %id.0, "could not load persisted pin pixels: {err}");
                None
            }
        }
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
        Pipeline::start_with_history(Arc::new(RefusingSelector), false)
            .expect("the worker should start")
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
                capture_id: None,
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
                capture_id: None,
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
                capture_id: None,
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
                capture_id: None,
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
                capture_id: None,
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
                capture_id: None,
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

    #[test]
    fn an_initial_pin_write_failure_requests_visible_rollback() {
        let (outcomes, outcome_rx) = channel();
        let card = CardId(9);
        let capture = CaptureId("capture-9".into());
        let vault = CaptureVault::new();
        vault.store(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(Vec::new()),
                    preview: None,
                },
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::AllDisplays,
                scale: ScaleFactor::IDENTITY,
                color_space: ColorSpace::Srgb,
                capture_id: Some(capture.clone()),
            },
        );
        let mut worker = Worker {
            outcomes,
            selector: Arc::new(RefusingSelector),
            store: None,
            vault,
        };
        let state = PinState::new(
            scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(10.0, 20.0),
                scrozz_core::LogicalSize::new(320.0, 180.0),
            ),
            scrozz_core::PinScale::ORIGINAL,
            None,
        );

        worker.pin_card(card, &capture, &state);

        match outcome_rx.recv().expect("rollback outcome") {
            Outcome::PinCreationFailed {
                capture: failed,
                error,
            } => {
                assert_eq!(failed, capture);
                assert!(error.to_string().contains("history is unavailable"));
            }
            other => panic!("expected pin creation failure, got {other:?}"),
        }
        assert!(worker.vault.get(card).is_none());
    }

    #[test]
    fn repeated_pin_updates_coalesce_to_the_latest_state() {
        let pending = PendingPinUpdates::default();
        let capture = CaptureId("capture-geometry".into());
        let first = PinState::new(
            scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(10.0, 20.0),
                scrozz_core::LogicalSize::new(320.0, 180.0),
            ),
            scrozz_core::PinScale::ORIGINAL,
            None,
        );
        let mut latest = first.clone();
        latest.frame.origin.x = 240.0;

        assert!(pending.queue(capture.clone(), Some(first)));
        assert!(!pending.queue(capture.clone(), Some(latest.clone())));

        let updates = pending.take();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates.get(&capture), Some(&Some(latest)));
    }

    #[test]
    fn terminal_unpin_cancels_geometry_waiting_for_a_flush() {
        let pending = PendingPinUpdates::default();
        let capture = CaptureId("capture-closing".into());
        let state = PinState::new(
            scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(10.0, 20.0),
                scrozz_core::LogicalSize::new(320.0, 180.0),
            ),
            scrozz_core::PinScale::ORIGINAL,
            None,
        );

        assert!(pending.queue(capture.clone(), Some(state)));
        pending.cancel(&capture);
        assert!(pending.take().is_empty());
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
