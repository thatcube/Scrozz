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

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    thread::JoinHandle,
    time::SystemTime,
};

use scrozz_annotate::Document;
use scrozz_core::{
    Capture, CaptureRequest, CaptureTarget, CursorMode, Error as CoreError, PinState,
};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, SystemClipboard};
use scrozz_store::{
    CaptureId, DocumentState, FrameHeader, History, NewCapture, Page, SearchQuery, SqliteStore,
};

use crate::{
    fault::{CliError, CliResult},
    gui::{
        action::CaptureKind,
        card::{
            Card, CardId, PIN_TEXTURE_MAX_EDGE, PinGeneration, PinnedCapture, SurfaceWaker,
            THUMBNAIL_MAX_EDGE, Thumbnail,
        },
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
        /// The identity the resulting card will carry, allocated up front so the
        /// main thread can correlate the answer with the request.
        card: CardId,
    },
    /// Accept pixels already captured by an interactive region/window selector.
    Captured {
        /// User-facing capture kind that produced the output.
        kind: CaptureKind,
        /// Identity allocated by the live capture stack.
        card: CardId,
        /// Authoritative pixels and provenance returned by the selector backend.
        capture: Capture,
    },
    /// Put a card's capture on the clipboard.
    Copy(CardId),
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
        /// Process-local identity generation for this pin attempt.
        generation: PinGeneration,
        /// Initial durable pin state.
        state: PinState,
    },
    /// Persist a changed pin or clear a closed one.
    SetPin {
        /// Durable history identity.
        capture: CaptureId,
        /// Generation whose state is being settled.
        generation: PinGeneration,
        /// `None` means the pin closed.
        state: Option<PinState>,
    },
    /// Clear a pin after every previously queued worker write, then acknowledge it.
    TerminalUnpin {
        /// Durable history identity.
        capture: CaptureId,
        /// Generation of the terminal close request.
        generation: PinGeneration,
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

#[derive(Debug, Clone, PartialEq)]
struct PendingPinUpdate {
    generation: PinGeneration,
    state: Option<PinState>,
}

#[derive(Default)]
struct PendingPinUpdates {
    states: Mutex<HashMap<CaptureId, PendingPinUpdate>>,
}

impl PendingPinUpdates {
    /// Keeps only the newest state per capture and reports whether the worker
    /// needs a wake-up.
    fn queue(
        &self,
        capture: CaptureId,
        generation: PinGeneration,
        state: Option<PinState>,
    ) -> bool {
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let needs_wakeup = states.is_empty();
        if states
            .get(&capture)
            .is_some_and(|pending| pending.generation > generation)
        {
            return false;
        }
        states.insert(capture, PendingPinUpdate { generation, state });
        needs_wakeup
    }

    fn take(&self) -> HashMap<CaptureId, PendingPinUpdate> {
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *states)
    }

    fn cancel_through(&self, capture: &CaptureId, generation: PinGeneration) {
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if states
            .get(capture)
            .is_some_and(|pending| pending.generation <= generation)
        {
            states.remove(capture);
        }
    }
}

/// What the capture thread produced.
#[derive(Debug)]
pub enum Outcome {
    /// A capture succeeded and is ready to show.
    Ready(Box<Card>),
    /// A capture failed. The main thread says why and shows nothing.
    Failed {
        /// Which card was expected.
        card: CardId,
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
    PinCreated {
        /// Source card retired only after the UI commits this settlement.
        card: CardId,
        /// Durable history identity.
        capture: CaptureId,
        /// Generation of the provisional pin this result belongs to.
        generation: PinGeneration,
        /// Pixels only; geometry and presentation remain UI-owned.
        texture: Option<Thumbnail>,
        /// Non-fatal high-resolution texture failure.
        warning: Option<CliError>,
    },
    /// The first durable pin write failed, so the visible viewport must roll back.
    PinCreationFailed {
        /// Durable pin identity to remove from the overlay.
        capture: CaptureId,
        /// Generation of the failed provisional pin.
        generation: PinGeneration,
        /// Explicit storage or identity error.
        error: CliError,
    },
    /// Persisting a pin update failed.
    PinPersistenceFailed {
        /// Capture whose sidecar could not be updated.
        capture: CaptureId,
        /// Generation whose write failed.
        generation: PinGeneration,
        /// Explicit storage error.
        error: CliError,
    },
}

/// A handle to the capture thread.
pub struct Pipeline {
    jobs: Sender<Job>,
    pending_pin_updates: Arc<PendingPinUpdates>,
    outcomes: Receiver<Outcome>,
    worker: Option<JoinHandle<()>>,
    next_card: u64,
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
    pub fn start() -> CliResult<Self> {
        Self::start_with_waker(None)
    }

    /// Starts the worker with an event-loop wake hook for completed work.
    pub fn start_with_waker(waker: Option<SurfaceWaker>) -> CliResult<Self> {
        let (jobs, job_rx) = channel();
        let (outcome_tx, outcomes) = channel();
        let pending_pin_updates = Arc::new(PendingPinUpdates::default());
        let worker_pin_updates = Arc::clone(&pending_pin_updates);

        let worker = std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || Worker::new(outcome_tx, waker).run(&job_rx, &worker_pin_updates))
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
        })
    }

    /// Allocates the next card identity.
    pub const fn allocate(&mut self) -> CardId {
        let id = CardId(self.next_card);
        self.next_card += 1;
        id
    }

    /// Posts a job. Returns `false` if the worker has gone.
    pub fn post(&self, job: Job) -> bool {
        if let Job::SetPin {
            capture,
            generation,
            state,
        } = job
        {
            if state.is_none() {
                // Closing is terminal for all geometry already queued by that
                // viewport. If the worker took an older state concurrently,
                // this direct job follows it in the channel and clears it.
                self.pending_pin_updates
                    .cancel_through(&capture, generation);
                return self
                    .jobs
                    .send(Job::SetPin {
                        capture,
                        generation,
                        state,
                    })
                    .is_ok();
            }
            if !self.pending_pin_updates.queue(capture, generation, state) {
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

    /// Feed a completed selector capture into the ordinary durable card pipeline.
    ///
    /// Region and window selectors own their platform-specific lifecycle, but
    /// their output must still receive history identity, a bounded texture, and
    /// the same Pin to Screen action as a fixed display capture.
    pub fn accept_capture(&mut self, kind: CaptureKind, capture: Capture) -> CliResult<CardId> {
        let card = self.allocate();
        self.jobs
            .send(Job::Captured {
                kind,
                card,
                capture,
            })
            .map_err(|_| {
                CliError::Core(CoreError::Platform(
                    "the capture worker stopped before it could accept selector pixels".into(),
                ))
            })?;
        Ok(card)
    }

    /// Clears a pin after all older worker jobs and waits for the durable result.
    pub fn terminal_unpin(&self, capture: CaptureId, generation: PinGeneration) -> CliResult<()> {
        self.pending_pin_updates
            .cancel_through(&capture, generation);
        let (reply, result) = channel();
        self.jobs
            .send(Job::TerminalUnpin {
                capture,
                generation,
                reply,
            })
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
struct Cached {
    bytes: Vec<u8>,
    capture_id: Option<CaptureId>,
}

struct Worker {
    outcomes: Sender<Outcome>,
    waker: Option<SurfaceWaker>,
    store: Option<SqliteStore>,
    cache: HashMap<CardId, Cached>,
    pin_generations: HashMap<CaptureId, PinGeneration>,
}

impl Worker {
    fn new(outcomes: Sender<Outcome>, waker: Option<SurfaceWaker>) -> Self {
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

        let mut worker = Self {
            outcomes,
            waker,
            store,
            cache: HashMap::new(),
            pin_generations: HashMap::new(),
        };
        worker.restore_existing_pins();
        worker
    }

    fn run(mut self, jobs: &Receiver<Job>, pending_pin_updates: &PendingPinUpdates) {
        while let Ok(job) = jobs.recv() {
            match job {
                Job::Capture { kind, card } => self.capture(kind, card),
                Job::Captured {
                    kind,
                    card,
                    capture,
                } => self.finish_capture(kind, card, capture),
                Job::Copy(card) => self.copy(card),
                Job::Save(card) => self.save(card),
                Job::Release(card) => {
                    self.cache.remove(&card);
                }
                Job::PinCard {
                    card,
                    capture,
                    generation,
                    state,
                } => {
                    if self.claim_pin_generation(&capture, generation) {
                        self.pin_card(card, &capture, generation, &state);
                    }
                }
                Job::SetPin {
                    capture,
                    generation,
                    state,
                } => {
                    if self.claim_pin_generation(&capture, generation) {
                        self.set_pin(&capture, generation, state.as_ref());
                    }
                }
                Job::TerminalUnpin {
                    capture,
                    generation,
                    reply,
                } => {
                    let result = if self.claim_pin_generation(&capture, generation) {
                        self.persist_pin(&capture, None)
                    } else {
                        Ok(())
                    };
                    let _ = reply.send(result);
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
                    for (capture, pending) in pending_pin_updates.take() {
                        if self.claim_pin_generation(&capture, pending.generation) {
                            self.set_pin(&capture, pending.generation, pending.state.as_ref());
                        }
                    }
                }
                Job::Stop => break,
            }
        }
        tracing::debug!("capture worker stopped");
    }

    fn capture(&mut self, kind: CaptureKind, card: CardId) {
        match self.take(kind, card) {
            Ok(built) => {
                self.emit(Outcome::Ready(Box::new(built)));
            }
            Err(error) => {
                tracing::warn!(%card, "capture failed: {error}");
                self.emit(Outcome::Failed { card, error });
            }
        }
    }

    fn finish_capture(&mut self, kind: CaptureKind, card: CardId, capture: Capture) {
        match self.card_from_capture(kind, card, capture) {
            Ok(built) => {
                self.emit(Outcome::Ready(Box::new(built)));
            }
            Err(error) => {
                tracing::warn!(%card, "selector capture could not enter the card pipeline: {error}");
                self.emit(Outcome::Failed { card, error });
            }
        }
    }

    fn take(&mut self, kind: CaptureKind, card: CardId) -> CliResult<Card> {
        // Through `platform`, not `scrozz_capture` directly, so the
        // SCROZZ_UNSTABLE_BACKENDS guard still applies to the GUI path.
        let backend = platform::capture_backend()?;
        let target = match kind {
            // The one capture with nothing to choose, so it needs nothing but a
            // backend. That is why it is the default hotkey.
            CaptureKind::Fullscreen => CaptureTarget::Display(backend.active_display()?.id),
            // Choosing a region or a window *on screen* is the selection
            // overlay's job, and it does not exist yet. Per D8 that is
            // explained rather than approximated: silently capturing the whole
            // display instead would be worse than refusing, because the user
            // would get a file they did not ask for. Naming the target is the
            // route that does work end to end — the result joins this capture
            // stack and can be pinned, which is the part they wanted.
            CaptureKind::Window => {
                return Err(CliError::not_implemented(
                    format!("choosing a {} on screen", kind.label()),
                    "scrozz-ui (the selection overlay); `scrozz capture --window <id or title>` \
                     names one without it, and its result joins this capture stack and can \
                     be pinned",
                ));
            }
            CaptureKind::Region => {
                return Err(CliError::not_implemented(
                    format!("choosing a {} on screen", kind.label()),
                    "scrozz-ui (the selection overlay); `scrozz capture --region X,Y,W,H` \
                     names one without it, and its result joins this capture stack and can \
                     be pinned",
                ));
            }
        };

        let request = CaptureRequest {
            target,
            cursor: CursorMode::Hidden,
            include_window_shadow: !kind.needs_selection(),
        };

        let capture = backend.capture(&request)?;
        self.card_from_capture(kind, card, capture)
    }

    fn card_from_capture(
        &mut self,
        kind: CaptureKind,
        card: CardId,
        capture: Capture,
    ) -> CliResult<Card> {
        let bytes = FrameEncoder::new().encode(&capture.frame, ImageFormat::Png)?;
        let thumbnail = Thumbnail::from_frame(&capture.frame, THUMBNAIL_MAX_EDGE).ok();
        let capture_id = self.remember(&capture);

        self.cache.insert(
            card,
            Cached {
                bytes,
                capture_id: capture_id.clone(),
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

    fn copy(&mut self, card: CardId) {
        // The round trip through PNG is deliberate — see the module docs — and
        // is also what will make "copy" work for a card whose capture arrived
        // over IPC, where the worker never held a `Frame` at all.
        let result = self
            .cached(card, "copy")
            .and_then(|cached| Ok(scrozz_export::decode(&cached.bytes)?))
            .and_then(|frame| {
                SystemClipboard::new().write_image_reporting(&frame)?;
                Ok("copied to the clipboard".to_owned())
            });
        self.answer(card, result);
    }

    fn save(&mut self, card: CardId) {
        let result = self.cached(card, "save").and_then(|cached| {
            let path = crate::output::export_default(&cached.bytes)?;
            Ok(format!("saved to {}", path.display()))
        });
        self.answer(card, result);
    }

    fn cached(&self, card: CardId, verb: &str) -> CliResult<&Cached> {
        self.cache
            .get(&card)
            .ok_or_else(|| CliError::usage(format!("{card} has no capture to {verb}")))
    }

    fn pin_card(
        &mut self,
        card: CardId,
        capture: &CaptureId,
        generation: PinGeneration,
        state: &PinState,
    ) {
        let cached_capture = self
            .cache
            .get(&card)
            .and_then(|cached| cached.capture_id.as_ref());
        if cached_capture != Some(capture) {
            let error = CliError::usage(format!(
                "{card} does not own persisted capture {}",
                capture.0
            ));
            self.emit(Outcome::PinCreationFailed {
                capture: capture.clone(),
                generation,
                error,
            });
            return;
        }
        let result = self.persist_pin(capture, Some(state));
        match result {
            Ok(()) => {
                let (texture, warning) = match self.load_pin_texture(capture) {
                    Ok(texture) => (Some(texture), None),
                    Err(error) => (None, Some(error)),
                };
                self.emit(Outcome::PinCreated {
                    card,
                    capture: capture.clone(),
                    generation,
                    texture,
                    warning,
                });
            }
            Err(error) => {
                self.emit(Outcome::PinCreationFailed {
                    capture: capture.clone(),
                    generation,
                    error,
                });
            }
        }
    }

    fn set_pin(
        &mut self,
        capture: &CaptureId,
        generation: PinGeneration,
        state: Option<&PinState>,
    ) {
        if let Err(error) = self.persist_pin(capture, state) {
            self.emit(Outcome::PinPersistenceFailed {
                capture: capture.clone(),
                generation,
                error,
            });
        }
    }

    fn claim_pin_generation(&mut self, capture: &CaptureId, generation: PinGeneration) -> bool {
        if self
            .pin_generations
            .get(capture)
            .is_some_and(|settled| *settled > generation)
        {
            return false;
        }
        self.pin_generations.insert(capture.clone(), generation);
        true
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
                self.emit(Outcome::PinReady(Box::new(pin)));
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
        let (source_width, source_height, scale, geometry_error) =
            safe_pin_source_geometry(&record.frame);
        let (mut texture, pixel_error) = match self.load_pin_texture(id) {
            Ok(texture) => (Some(texture), None),
            Err(error) => {
                tracing::warn!(capture = %id.0, "could not load persisted pin pixels: {error}");
                (None, Some(error.to_string()))
            }
        };
        if geometry_error.is_some() {
            texture = None;
        }
        let content_error = [geometry_error, pixel_error]
            .into_iter()
            .flatten()
            .reduce(|left, right| format!("{left}; {right}"));
        Some(PinnedCapture {
            id: id.clone(),
            name: record
                .window_title
                .clone()
                .or(record.app_name.clone())
                .unwrap_or_else(|| format!("Capture {}", id.0)),
            provenance: record.provenance,
            source_width,
            source_height,
            scale,
            state,
            texture,
            content_error,
        })
    }

    fn load_pin_texture(&mut self, id: &CaptureId) -> CliResult<Thumbnail> {
        let store = self.store.as_mut().ok_or_else(|| {
            CliError::Core(CoreError::Storage(
                "history is unavailable, so pin pixels cannot be loaded".into(),
            ))
        })?;
        match store.document(id) {
            Ok(Some(DocumentState::Complete(document))) => {
                Thumbnail::from_frame(&document.source.frame, PIN_TEXTURE_MAX_EDGE)
                    .map_err(CliError::from)
            }
            Ok(Some(DocumentState::ImageEvicted(_))) | Ok(None) => {
                Err(CliError::Core(CoreError::Storage(format!(
                    "pinned capture {} still has recoverable state, but its source pixels are unavailable",
                    id.0
                ))))
            }
            Err(err) => Err(CliError::from(err)),
        }
    }

    fn answer(&self, card: CardId, result: CliResult<String>) {
        let message = match result {
            Ok(detail) => Outcome::Done { card, detail },
            Err(error) => Outcome::Refused { card, error },
        };
        self.emit(message);
    }

    fn emit(&self, outcome: Outcome) {
        if self.outcomes.send(outcome).is_ok()
            && let Some(waker) = &self.waker
        {
            waker();
        }
    }
}

fn safe_pin_source_geometry(frame: &FrameHeader) -> (u32, u32, f64, Option<String>) {
    let width = checked_source_dimension(frame.size.width);
    let height = checked_source_dimension(frame.size.height);
    let scale = frame.scale.get();
    if let (Some(width), Some(height)) = (width, height)
        && scale.is_finite()
        && scale > 0.0
    {
        return (width, height, scale, None);
    }
    (
        420,
        180,
        1.0,
        Some(
            "Pinned capture metadata contains invalid physical dimensions; a safe error viewport was used"
                .into(),
        ),
    )
}

fn checked_source_dimension(value: f64) -> Option<u32> {
    (value.is_finite() && value >= 1.0 && value <= f64::from(u32::MAX))
        .then(|| value.round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{
        ColorSpace, Frame, PhysicalSize, PixelFormat, Provenance, ScaleFactor, WindowId,
    };

    fn sample_capture(provenance: Provenance) -> Capture {
        Capture {
            frame: Frame {
                data: vec![255; 4 * 4 * 4],
                size: PhysicalSize::new(4.0, 4.0),
                stride: 16,
                format: PixelFormat::Rgba8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::new(2.0),
            },
            provenance,
            target: CaptureTarget::Window(WindowId("selector-window".into())),
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("scrozz-pin-{name}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn a_pipeline_hands_out_distinct_card_identities() {
        let mut pipeline = Pipeline::start().expect("the worker should start");
        let first = pipeline.allocate();
        let second = pipeline.allocate();
        assert_ne!(first, second);
        assert_eq!(first, CardId(1));
    }

    #[test]
    fn polling_an_idle_pipeline_does_not_block() {
        let pipeline = Pipeline::start().expect("the worker should start");
        assert!(pipeline.poll().is_none());
    }

    #[test]
    fn completed_worker_work_wakes_the_window_event_loop() {
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&wakes);
        let waker: SurfaceWaker = Arc::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        let pipeline = Pipeline::start_with_waker(Some(waker)).expect("worker");
        assert!(pipeline.post(Job::Copy(CardId(404))));
        let _ = wait_for(&pipeline);
        assert!(wakes.load(std::sync::atomic::Ordering::Relaxed) > 0);
    }

    #[test]
    fn a_pipeline_stops_cleanly_and_twice_is_harmless() {
        // Drop also stops it, so the second call must be a no-op rather than a
        // join on an already-joined handle.
        let mut pipeline = Pipeline::start().expect("the worker should start");
        pipeline.stop();
        pipeline.stop();
    }

    #[test]
    fn copying_a_card_that_was_never_captured_is_refused_not_ignored() {
        let pipeline = Pipeline::start().expect("the worker should start");
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
        let pipeline = Pipeline::start().expect("the worker should start");
        assert!(pipeline.post(Job::Save(CardId(7))));

        match wait_for(&pipeline) {
            Some(Outcome::Refused { card, .. }) => assert_eq!(card, CardId(7)),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn releasing_an_unknown_card_is_harmless() {
        let pipeline = Pipeline::start().expect("the worker should start");
        assert!(pipeline.post(Job::Release(CardId(1))));
        assert!(pipeline.post(Job::Release(CardId(1))));
    }

    #[test]
    fn selector_outputs_receive_durable_identity_and_pin_ready_provenance() {
        let root = scratch("selector-output");
        let store = SqliteStore::open_ephemeral(&root).expect("ephemeral history");
        let (outcomes, _outcome_rx) = channel();
        let mut worker = Worker {
            outcomes,
            waker: None,
            store: Some(store),
            cache: HashMap::new(),
            pin_generations: HashMap::new(),
        };

        let card = worker
            .card_from_capture(
                CaptureKind::Window,
                CardId(44),
                sample_capture(Provenance::Window),
            )
            .expect("selector output enters card pipeline");

        assert!(
            card.capture_id.is_some(),
            "Pin to Screen needs durable identity"
        );
        assert_eq!(card.provenance, Provenance::Window);
        assert!(
            card.thumbnail.is_some(),
            "the pin must begin with real pixels"
        );
        drop(worker);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_persisted_dimensions_use_a_bounded_error_viewport() {
        let mut header = FrameHeader::of(&sample_capture(Provenance::Window).frame);
        header.size = PhysicalSize::new(f64::NAN, f64::INFINITY);

        let (width, height, scale, error) = safe_pin_source_geometry(&header);
        assert_eq!((width, height, scale), (420, 180, 1.0));
        assert!(error.is_some());
    }

    #[test]
    fn missing_pin_pixels_surface_an_error_without_clearing_recoverable_state() {
        let root = scratch("missing-pin-pixels");
        let store = SqliteStore::open_ephemeral(&root).expect("ephemeral history");
        let (outcomes, _outcome_rx) = channel();
        let mut worker = Worker {
            outcomes,
            waker: None,
            store: Some(store),
            cache: HashMap::new(),
            pin_generations: HashMap::new(),
        };
        let card = worker
            .card_from_capture(
                CaptureKind::Window,
                CardId(45),
                sample_capture(Provenance::Window),
            )
            .expect("stored capture");
        let id = card.capture_id.expect("durable identity");
        let state = PinState::new(
            scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(10.0, 20.0),
                scrozz_core::LogicalSize::new(320.0, 180.0),
            ),
            scrozz_core::PinScale::ORIGINAL,
            None,
        );
        let store = worker.store.as_mut().expect("store");
        store
            .set_screen_pin(&id, Some(&state))
            .expect("persist pin state");
        let record = store.record(&id).expect("record read").expect("record");
        let scrozz_store::ImageState::Present { hash, .. } = record.image else {
            panic!("new capture pixels must be present");
        };
        let blob = store.layout().blob_path(&hash).expect("blob path");
        std::fs::remove_file(blob).expect("simulate unreadable pixels");

        let restored = worker.load_pin(&id).expect("pin metadata survives");
        assert!(restored.texture.is_none());
        assert!(
            restored
                .content_error
                .as_deref()
                .is_some_and(|error| error.contains("pixels"))
        );
        assert_eq!(
            worker
                .store
                .as_ref()
                .expect("store")
                .record(&id)
                .expect("record")
                .expect("present")
                .screen_pin,
            Some(state)
        );
        drop(worker);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_initial_pin_write_failure_requests_visible_rollback() {
        let (outcomes, outcome_rx) = channel();
        let card = CardId(9);
        let capture = CaptureId("capture-9".into());
        let mut worker = Worker {
            outcomes,
            waker: None,
            store: None,
            cache: HashMap::from([(
                card,
                Cached {
                    bytes: Vec::new(),
                    capture_id: Some(capture.clone()),
                },
            )]),
            pin_generations: HashMap::new(),
        };
        let state = PinState::new(
            scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(10.0, 20.0),
                scrozz_core::LogicalSize::new(320.0, 180.0),
            ),
            scrozz_core::PinScale::ORIGINAL,
            None,
        );

        worker.pin_card(card, &capture, PinGeneration(1), &state);

        match outcome_rx.recv().expect("rollback outcome") {
            Outcome::PinCreationFailed {
                capture: failed,
                generation,
                error,
            } => {
                assert_eq!(failed, capture);
                assert_eq!(generation, PinGeneration(1));
                assert!(error.to_string().contains("history is unavailable"));
            }
            other => panic!("expected pin creation failure, got {other:?}"),
        }
        assert!(
            worker.cache.contains_key(&card),
            "the source card stays fully usable after rollback"
        );
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

        assert!(pending.queue(capture.clone(), PinGeneration(1), Some(first)));
        assert!(!pending.queue(capture.clone(), PinGeneration(1), Some(latest.clone())));

        let updates = pending.take();
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates.get(&capture),
            Some(&PendingPinUpdate {
                generation: PinGeneration(1),
                state: Some(latest),
            })
        );
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

        assert!(pending.queue(capture.clone(), PinGeneration(1), Some(state)));
        pending.cancel_through(&capture, PinGeneration(2));
        assert!(pending.take().is_empty());
    }

    #[test]
    fn stale_updates_cannot_replace_a_newer_pending_generation() {
        let pending = PendingPinUpdates::default();
        let capture = CaptureId("capture-repinned".into());
        let newer = PinState::new(
            scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(50.0, 60.0),
                scrozz_core::LogicalSize::new(320.0, 180.0),
            ),
            scrozz_core::PinScale::ORIGINAL,
            None,
        );
        let mut stale = newer.clone();
        stale.frame.origin.x = 1.0;

        assert!(pending.queue(capture.clone(), PinGeneration(3), Some(newer.clone())));
        assert!(!pending.queue(capture.clone(), PinGeneration(2), Some(stale)));
        pending.cancel_through(&capture, PinGeneration(2));

        assert_eq!(
            pending.take().get(&capture),
            Some(&PendingPinUpdate {
                generation: PinGeneration(3),
                state: Some(newer),
            })
        );
    }

    #[test]
    fn a_stale_terminal_settlement_cannot_override_a_new_pin() {
        let (outcomes, _outcome_rx) = channel();
        let capture = CaptureId("capture-generation".into());
        let mut worker = Worker {
            outcomes,
            waker: None,
            store: None,
            cache: HashMap::new(),
            pin_generations: HashMap::new(),
        };

        assert!(worker.claim_pin_generation(&capture, PinGeneration(3)));
        assert!(!worker.claim_pin_generation(&capture, PinGeneration(2)));
        assert!(worker.claim_pin_generation(&capture, PinGeneration(3)));
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
