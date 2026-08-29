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
        Mutex,
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    thread::JoinHandle,
    time::{Duration, SystemTime},
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
        server::Request,
    },
    platform,
};

const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(2);

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
        /// One-way cancellation for queued or in-flight frame acquisition.
        acquisition_cancellation: scrozz_capture::CaptureCancellation,
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
    /// Execute a command forwarded by another process and answer its socket.
    Forward(Request),
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
    /// A forwarded command finished and its caller has been answered.
    Forwarded(Option<crate::cli::Command>),
}

/// A handle to the capture thread.
pub struct Pipeline {
    jobs: Sender<Job>,
    outcomes: Receiver<Outcome>,
    worker: Option<JoinHandle<()>>,
    worker_done: Receiver<()>,
    capture_cancellation: scrozz_capture::CaptureCancellation,
    next_card: u64,
    scrolling_cancellation: AtomicCancellation,
    active_scrolling_acquisition: Mutex<Option<scrozz_capture::CaptureCancellation>>,
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
        let (worker_done_tx, worker_done) = channel();
        let capture_cancellation = scrozz_capture::CaptureCancellation::new();
        let scrolling_cancellation = AtomicCancellation::default();
        let worker_capture_cancellation = capture_cancellation.clone();
        let worker_scrolling_cancellation = scrolling_cancellation.clone();

        let worker = std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || {
                Worker::new(
                    outcome_tx,
                    worker_capture_cancellation,
                    worker_scrolling_cancellation,
                )
                .run(&job_rx);
                let _ = worker_done_tx.send(());
            })
            .map_err(|err| {
                CliError::Core(CoreError::Platform(format!(
                    "could not start the capture worker: {err}"
                )))
            })?;

        Ok(Self {
            jobs,
            outcomes,
            worker: Some(worker),
            worker_done,
            capture_cancellation,
            next_card: 1,
            scrolling_cancellation,
            active_scrolling_acquisition: Mutex::new(None),
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
        match self.jobs.send(job) {
            Ok(()) => true,
            Err(std::sync::mpsc::SendError(Job::Forward(request))) => {
                // Do not leave the forwarding client blocked on an unexplained
                // EOF if the worker died between accept and enqueue.
                let cancellation = scrozz_capture::CaptureCancellation::new();
                cancellation.cancel();
                let _ = request.serve_with_cancellation(&cancellation);
                false
            }
            Err(std::sync::mpsc::SendError(_)) => false,
        }
    }

    /// Posts a scrolling job with a token that Abort can cancel even while the
    /// job is queued or waiting in native frame acquisition.
    pub fn post_scrolling(
        &self,
        axis: ScrollAxis,
        card: CardId,
        target: Box<ScrollingTarget>,
    ) -> bool {
        self.scrolling_cancellation.reset();
        let acquisition_cancellation = scrozz_capture::CaptureCancellation::new();
        *self
            .active_scrolling_acquisition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(acquisition_cancellation.clone());
        let posted = self.post(Job::Scrolling {
            axis,
            card,
            target,
            acquisition_cancellation: acquisition_cancellation.clone(),
        });
        if !posted {
            acquisition_cancellation.cancel();
            self.active_scrolling_acquisition
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
        }
        posted
    }

    /// Ask the active scrolling session to keep its partial image or abort it.
    pub fn cancel_scrolling(&self, action: CancelAction) {
        self.scrolling_cancellation.cancel(action);
        if action == CancelAction::Abort
            && let Some(cancellation) = self
                .active_scrolling_acquisition
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
        {
            cancellation.cancel();
        }
    }

    /// Takes one finished piece of work, if there is one. Never blocks.
    pub fn poll(&self) -> Option<Outcome> {
        self.outcomes.try_recv().ok()
    }

    /// Cancels interactive acquisition, then stops and joins the worker.
    ///
    /// Called from `Drop`, but exposed so a host can shut down deterministically
    /// rather than at an unspecified point during teardown. Cancelling first
    /// closes an active Wayland ScreenCast session and dismisses its picker.
    pub fn stop(&mut self) {
        self.capture_cancellation.cancel();
        self.cancel_scrolling(CancelAction::Abort);
        let _ = self.jobs.send(Job::Stop);
        if let Some(worker) = self.worker.take() {
            match self.worker_done.recv_timeout(WORKER_STOP_TIMEOUT) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                    if worker.join().is_err() {
                        tracing::warn!("capture worker panicked during shutdown");
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    tracing::warn!(
                        timeout_ms = WORKER_STOP_TIMEOUT.as_millis(),
                        "capture worker did not stop in time; detaching the in-flight operation"
                    );
                    drop(worker);
                }
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
    capture_cancellation: scrozz_capture::CaptureCancellation,
    scrolling_cancellation: AtomicCancellation,
}

impl Worker {
    fn new(
        outcomes: Sender<Outcome>,
        capture_cancellation: scrozz_capture::CaptureCancellation,
        scrolling_cancellation: AtomicCancellation,
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
            store,
            cache: HashMap::new(),
            pending: HashMap::new(),
            capture_cancellation,
            scrolling_cancellation,
        }
    }

    fn run(mut self, jobs: &Receiver<Job>) {
        while let Ok(job) = jobs.recv() {
            if self.capture_cancellation.is_cancelled() {
                match job {
                    // The caller is blocked waiting for a protocol response. Run
                    // only the cheap parse/response path, which observes the
                    // already-cancelled token before dispatching any command.
                    Job::Forward(request) => self.forward(request),
                    Job::Stop => break,
                    Job::Capture { .. }
                    | Job::Scrolling { .. }
                    | Job::Copy(_)
                    | Job::Save(_)
                    | Job::Release(_)
                    | Job::Accept(_)
                    | Job::Discard { .. } => {}
                }
                continue;
            }

            match job {
                Job::Capture { kind, card } => self.capture(kind, card, None),
                Job::Scrolling {
                    axis,
                    card,
                    target,
                    acquisition_cancellation,
                } => {
                    self.capture(
                        CaptureKind::Scrolling,
                        card,
                        Some((axis, *target, acquisition_cancellation)),
                    );
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
                Job::Forward(request) => self.forward(request),
                Job::Stop => break,
            }
        }
        tracing::debug!("capture worker stopped");
    }

    fn capture(
        &mut self,
        kind: CaptureKind,
        card: CardId,
        scrolling: Option<(
            ScrollAxis,
            ScrollingTarget,
            scrozz_capture::CaptureCancellation,
        )>,
    ) {
        match self.take(kind, card, scrolling) {
            Ok(built) => {
                if kind == CaptureKind::Scrolling
                    && self.scrolling_cancellation.requested() == Some(CancelAction::Abort)
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

    fn forward(&self, request: Request) {
        let command = request.serve_with_cancellation(&self.capture_cancellation);
        let _ = self.outcomes.send(Outcome::Forwarded(command));
    }

    fn take(
        &mut self,
        kind: CaptureKind,
        card: CardId,
        scrolling: Option<(
            ScrollAxis,
            ScrollingTarget,
            scrozz_capture::CaptureCancellation,
        )>,
    ) -> CliResult<Card> {
        if let Some((_, _, cancellation)) = &scrolling {
            self.fail_if_aborted(kind)?;
            if cancellation.is_cancelled() {
                return Err(CliError::Core(CoreError::Cancelled));
            }
        }
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
                .map(|(_, target, _)| target.capture_target())
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
            let (axis, target, acquisition_cancellation) = scrolling.ok_or_else(|| {
                CliError::Core(CoreError::InvalidRequest(
                    "a scrolling pipeline job must name its axis and target".to_owned(),
                ))
            })?;
            let outcomes = self.outcomes.clone();
            crate::commands::scrolling_capture_target_with_cancellation(
                target,
                axis,
                &mut self.scrolling_cancellation,
                &acquisition_cancellation,
                move |progress| {
                    let _ = outcomes.send(Outcome::Progress { card, progress });
                },
            )?
        } else {
            platform::capture_with_cancellation(&request, &self.capture_cancellation)?
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
            && self.scrolling_cancellation.requested() == Some(CancelAction::Abort)
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
            && self.scrolling_cancellation.requested() == Some(CancelAction::Abort)
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
        assert!(!pipeline.capture_cancellation.is_cancelled());
        pipeline.stop();
        assert!(pipeline.capture_cancellation.is_cancelled());
        assert_eq!(
            pipeline.scrolling_cancellation.requested(),
            Some(CancelAction::Abort)
        );
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
    fn abort_cancels_the_active_scrolling_acquisition_token() {
        let pipeline = Pipeline::start().expect("the worker should start");
        let acquisition = scrozz_capture::CaptureCancellation::new();
        *pipeline
            .active_scrolling_acquisition
            .lock()
            .expect("acquisition slot") = Some(acquisition.clone());

        pipeline.cancel_scrolling(CancelAction::Abort);

        assert!(acquisition.is_cancelled());
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
