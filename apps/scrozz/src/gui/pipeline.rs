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
use scrozz_store::{CaptureId, DocumentState, History, NewCapture, Page, SearchQuery, SqliteStore};

use crate::{
    fault::{CliError, CliResult},
    gui::{
        action::CaptureKind,
        card::{Card, CardId, PIN_TEXTURE_MAX_EDGE, PinnedCapture, THUMBNAIL_MAX_EDGE, Thumbnail},
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
        let (jobs, job_rx) = channel();
        let (outcome_tx, outcomes) = channel();
        let pending_pin_updates = Arc::new(PendingPinUpdates::default());
        let worker_pin_updates = Arc::clone(&pending_pin_updates);

        let worker = std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || Worker::new(outcome_tx).run(&job_rx, &worker_pin_updates))
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
struct Cached {
    bytes: Vec<u8>,
    capture_id: Option<CaptureId>,
}

struct Worker {
    outcomes: Sender<Outcome>,
    store: Option<SqliteStore>,
    cache: HashMap<CardId, Cached>,
}

impl Worker {
    fn new(outcomes: Sender<Outcome>) -> Self {
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
            store,
            cache: HashMap::new(),
        };
        worker.restore_existing_pins();
        worker
    }

    fn run(mut self, jobs: &Receiver<Job>, pending_pin_updates: &PendingPinUpdates) {
        while let Ok(job) = jobs.recv() {
            match job {
                Job::Capture { kind, card } => self.capture(kind, card),
                Job::Copy(card) => self.copy(card),
                Job::Save(card) => self.save(card),
                Job::Release(card) => {
                    self.cache.remove(&card);
                }
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

    fn capture(&mut self, kind: CaptureKind, card: CardId) {
        match self.take(kind, card) {
            Ok(built) => {
                let _ = self.outcomes.send(Outcome::Ready(Box::new(built)));
            }
            Err(error) => {
                tracing::warn!(%card, "capture failed: {error}");
                let _ = self.outcomes.send(Outcome::Failed { card, error });
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
            // Choosing a region or a window is the selection overlay's job, and
            // per D8 a missing capability is explained rather than approximated.
            // Silently capturing the whole display instead would be worse than
            // refusing: the user would get a file they did not ask for.
            CaptureKind::Region | CaptureKind::Window => {
                return Err(CliError::not_implemented(
                    format!("choosing a {} on screen", kind.label()),
                    "scrozz-ui (the selection overlay); \
                     `scrozz capture --region X,Y,W,H` takes one without it",
                ));
            }
        };

        let request = CaptureRequest {
            target,
            cursor: CursorMode::Hidden,
            include_window_shadow: !kind.needs_selection(),
        };

        let capture = backend.capture(&request)?;
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

    fn pin_card(&mut self, card: CardId, capture: &CaptureId, state: &PinState) {
        let cached_capture = self
            .cache
            .get(&card)
            .and_then(|cached| cached.capture_id.as_ref());
        if cached_capture != Some(capture) {
            let error = CliError::usage(format!(
                "{card} does not own persisted capture {}",
                capture.0
            ));
            let _ = self.outcomes.send(Outcome::PinCreationFailed {
                capture: capture.clone(),
                error,
            });
            self.cache.remove(&card);
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
                self.cache.remove(&card);
            }
            Err(error) => {
                let _ = self.outcomes.send(Outcome::PinCreationFailed {
                    capture: capture.clone(),
                    error,
                });
                self.cache.remove(&card);
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn an_initial_pin_write_failure_requests_visible_rollback() {
        let (outcomes, outcome_rx) = channel();
        let card = CardId(9);
        let capture = CaptureId("capture-9".into());
        let mut worker = Worker {
            outcomes,
            store: None,
            cache: HashMap::from([(
                card,
                Cached {
                    bytes: Vec::new(),
                    capture_id: Some(capture.clone()),
                },
            )]),
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
        assert!(!worker.cache.contains_key(&card));
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
