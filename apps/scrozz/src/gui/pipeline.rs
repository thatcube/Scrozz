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
    sync::mpsc::{Receiver, Sender, channel},
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use scrozz_annotate::Document;
use scrozz_core::{
    Capture, CaptureRequest, CaptureTarget, CursorMode, Error as CoreError, ScrollAxis,
};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, SystemClipboard};
use scrozz_stitch::{AtomicCancellation, CancelAction, Progress};
use scrozz_store::{CaptureId, History, NewCapture, SqliteStore};

use crate::{
    commands::ScrollingTarget,
    fault::{CliError, CliResult},
    gui::{
        action::CaptureKind,
        card::{Card, CardId, THUMBNAIL_MAX_EDGE, Thumbnail},
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
    /// Take and assemble repeated frames along one explicit axis.
    Scrolling {
        /// Direction the stitched image grows.
        axis: ScrollAxis,
        /// Identity reserved for the resulting card.
        card: CardId,
        /// Window identity selected before the HUD and geometry refreshed when
        /// the user chose an axis.
        target: Box<ScrollingTarget>,
    },
    /// Put a card's capture on the clipboard.
    Copy(CardId),
    /// Write a card's capture to the configured folder.
    Save(CardId),
    /// Forget a card's cached bytes. The card itself is the surface's business.
    Release(CardId),
    /// Commit a completed scrolling capture after the HUD accepts it.
    Accept(CardId),
    /// Remove a capture that lost a race with an explicit scrolling abort.
    Discard {
        /// Session-local card identity.
        card: CardId,
        /// History identity, when persistence completed before the abort.
        capture: Option<CaptureId>,
    },
    /// Finish, so the thread can be joined.
    Stop,
}

/// What the capture thread produced.
#[derive(Debug)]
pub enum Outcome {
    /// A scrolling capture reached a meaningful frame boundary.
    Progress {
        /// Which in-flight card the update belongs to.
        card: CardId,
        /// Session status suitable for the HUD and diagnostics.
        progress: Progress,
    },
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
}

/// A handle to the capture thread.
pub struct Pipeline {
    jobs: Sender<Job>,
    outcomes: Receiver<Outcome>,
    worker: Option<JoinHandle<()>>,
    next_card: u64,
    cancellation: AtomicCancellation,
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
        let cancellation = AtomicCancellation::default();
        let worker_cancellation = cancellation.clone();

        let worker = std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || Worker::new(outcome_tx, worker_cancellation).run(&job_rx))
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
            cancellation,
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
        if matches!(&job, Job::Scrolling { .. }) {
            self.cancellation.reset();
        }
        self.jobs.send(job).is_ok()
    }

    /// Ask the active scrolling session to keep its partial image or abort it.
    pub fn cancel_scrolling(&self, action: CancelAction) {
        self.cancellation.cancel(action);
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
        self.cancellation.cancel(CancelAction::Abort);
        let _ = self.jobs.send(Job::Stop);
        if let Some(worker) = self.worker.take() {
            let deadline = Instant::now() + Duration::from_millis(750);
            while !worker.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                // Portal permission calls are owned by the OS and cannot be
                // interrupted synchronously. Dropping the handle detaches this
                // worker so quitting the menu app never waits on that dialog.
                tracing::warn!(
                    "capture worker did not stop promptly; detaching it during shutdown"
                );
            }
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
}

struct Worker {
    outcomes: Sender<Outcome>,
    store: Option<SqliteStore>,
    cache: HashMap<CardId, Cached>,
    pending: HashMap<CardId, Capture>,
    cancellation: AtomicCancellation,
}

impl Worker {
    fn new(outcomes: Sender<Outcome>, cancellation: AtomicCancellation) -> Self {
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
            store,
            cache: HashMap::new(),
            pending: HashMap::new(),
            cancellation,
        }
    }

    fn run(mut self, jobs: &Receiver<Job>) {
        while let Ok(job) = jobs.recv() {
            match job {
                Job::Capture { kind, card } => self.capture(kind, card, None),
                Job::Scrolling { axis, card, target } => {
                    self.capture(CaptureKind::Scrolling, card, Some((axis, *target)));
                }
                Job::Copy(card) => self.copy(card),
                Job::Save(card) => self.save(card),
                Job::Release(card) => {
                    self.cache.remove(&card);
                }
                Job::Accept(card) => self.accept(card),
                Job::Discard { card, capture } => {
                    if let Err(error) = self.discard(card, capture.as_ref()) {
                        let _ = self.outcomes.send(Outcome::Refused { card, error });
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
        scrolling: Option<(ScrollAxis, ScrollingTarget)>,
    ) {
        match self.take(kind, card, scrolling) {
            Ok(built) => {
                if kind == CaptureKind::Scrolling
                    && self.cancellation.requested() == Some(CancelAction::Abort)
                {
                    let error = self
                        .discard(built.id, built.capture_id.as_ref())
                        .err()
                        .unwrap_or(CliError::Core(CoreError::Cancelled));
                    let _ = self.outcomes.send(Outcome::Failed { card, error });
                } else {
                    let _ = self.outcomes.send(Outcome::Ready(Box::new(built)));
                }
            }
            Err(error) => {
                tracing::warn!(%card, "capture failed: {error}");
                let _ = self.outcomes.send(Outcome::Failed { card, error });
            }
        }
    }

    fn take(
        &mut self,
        kind: CaptureKind,
        card: CardId,
        scrolling: Option<(ScrollAxis, ScrollingTarget)>,
    ) -> CliResult<Card> {
        // Through `platform`, not `scrozz_capture` directly, so test builds stay
        // isolated from the developer's real screen and backend errors keep the
        // CLI/GUI fault contract.
        let backend = platform::capture_backend()?;
        let target = match kind {
            // The one capture with nothing to choose, so it needs nothing but a
            // backend. That is why it is the default hotkey.
            CaptureKind::Fullscreen => CaptureTarget::Display(backend.active_display()?.id),
            CaptureKind::Scrolling => scrolling
                .as_ref()
                .map(|(_, target)| target.capture_target())
                .ok_or_else(|| {
                    CliError::Core(CoreError::InvalidRequest(
                        "a scrolling pipeline job must carry its snapshotted target".to_owned(),
                    ))
                })?,
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

        let capture = if kind == CaptureKind::Scrolling {
            let (axis, target) = scrolling.ok_or_else(|| {
                CliError::Core(CoreError::InvalidRequest(
                    "a scrolling pipeline job must name its axis and target".to_owned(),
                ))
            })?;
            let outcomes = self.outcomes.clone();
            crate::commands::scrolling_capture_target_with(
                target,
                axis,
                &mut self.cancellation,
                move |progress| {
                    let _ = outcomes.send(Outcome::Progress { card, progress });
                },
            )?
        } else {
            backend.capture(&request)?
        };
        self.fail_if_aborted(kind)?;
        let bytes = FrameEncoder::new().encode(&capture.frame, ImageFormat::Png)?;
        self.fail_if_aborted(kind)?;
        let thumbnail = Thumbnail::from_frame(&capture.frame, THUMBNAIL_MAX_EDGE).ok();
        self.fail_if_aborted(kind)?;
        let source_width = capture.frame.width();
        let source_height = capture.frame.height();
        let scale = capture.frame.scale.get();
        let capture_id = if kind == CaptureKind::Scrolling {
            self.pending.insert(card, capture);
            None
        } else {
            self.remember(capture)
        };
        if kind == CaptureKind::Scrolling
            && self.cancellation.requested() == Some(CancelAction::Abort)
        {
            self.discard(card, capture_id.as_ref())?;
            return Err(CliError::Core(CoreError::Cancelled));
        }

        self.cache.insert(card, Cached { bytes });

        Ok(Card {
            id: card,
            capture_id,
            kind,
            source_width,
            source_height,
            scale,
            thumbnail,
            // History persistence is internal and does not count as an export.
            // A visible file is created only when the user presses Save; writing
            // one here made Save create a duplicate a few seconds later.
            written: Vec::new(),
            taken_at: SystemTime::now(),
        })
    }

    /// Persists a capture, or explains in the log why it was not.
    fn remember(&mut self, capture: Capture) -> Option<CaptureId> {
        let store = self.store.as_mut()?;
        let document = Document::new(capture);
        match store.insert(NewCapture::new(&document)) {
            Ok(id) => Some(id),
            Err(err) => {
                tracing::warn!("could not add the capture to history: {err}");
                None
            }
        }
    }

    fn accept(&mut self, card: CardId) {
        if let Some(capture) = self.pending.remove(&card) {
            self.remember(capture);
        }
    }

    fn fail_if_aborted(&self, kind: CaptureKind) -> CliResult<()> {
        if kind == CaptureKind::Scrolling
            && self.cancellation.requested() == Some(CancelAction::Abort)
        {
            return Err(CliError::Core(CoreError::Cancelled));
        }
        Ok(())
    }

    fn discard(&mut self, card: CardId, capture: Option<&CaptureId>) -> CliResult<()> {
        self.pending.remove(&card);
        self.cache.remove(&card);
        if let (Some(store), Some(capture)) = (self.store.as_mut(), capture) {
            store.delete(capture)?;
        }
        Ok(())
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
