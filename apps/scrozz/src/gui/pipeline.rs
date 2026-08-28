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
        Arc,
        mpsc::{Receiver, Sender, channel},
    },
    thread::JoinHandle,
    time::SystemTime,
};

use scrozz_annotate::Document;
use scrozz_core::{
    Capture, CaptureRequest, CaptureTarget, CursorMode, Error as CoreError, SelectionMode,
    SelectionOptions,
};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, SystemClipboard};
use scrozz_store::{CaptureId, History, NewCapture, SqliteStore};

use crate::{
    fault::{CliError, CliResult},
    gui::{
        action::{CaptureKind, CaptureOrigin},
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
        /// Where the request entered the app.
        origin: CaptureOrigin,
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
    /// Finish, so the thread can be joined.
    Stop,
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
}

/// A handle to the capture thread.
pub struct Pipeline {
    jobs: Sender<Job>,
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
    pub fn start(selector: Arc<dyn CaptureSelector>) -> CliResult<Self> {
        let (jobs, job_rx) = channel();
        let (outcome_tx, outcomes) = channel();

        let worker = std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || Worker::new(outcome_tx, selector).run(&job_rx))
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
struct Cached {
    bytes: Vec<u8>,
}

struct Worker {
    outcomes: Sender<Outcome>,
    selector: Arc<dyn CaptureSelector>,
    store: Option<SqliteStore>,
    cache: HashMap<CardId, Cached>,
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
    fn new(outcomes: Sender<Outcome>, selector: Arc<dyn CaptureSelector>) -> Self {
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
            cache: HashMap::new(),
        }
    }

    fn run(mut self, jobs: &Receiver<Job>) {
        while let Ok(job) = jobs.recv() {
            match job {
                Job::Capture { kind, card, origin } => self.capture(kind, card, origin),
                Job::Copy(card) => self.copy(card),
                Job::Save(card) => self.save(card),
                Job::Release(card) => {
                    self.cache.remove(&card);
                }
                Job::Stop => break,
            }
        }
        tracing::debug!("capture worker stopped");
    }

    fn capture(&mut self, kind: CaptureKind, card: CardId, origin: CaptureOrigin) {
        tracing::debug!(
            %card,
            capture = kind.label(),
            origin = origin.label(),
            "capture job started"
        );
        let mut lifecycle = CaptureLifecycle::new(Arc::clone(&self.selector));
        let result = self.take(kind, card, &mut lifecycle);
        match result {
            Ok(built) => {
                let _ = self.outcomes.send(Outcome::Ready(Box::new(built)));
            }
            Err(error) if error.is_cancellation() => {
                tracing::debug!(%card, origin = origin.label(), "capture selection cancelled");
            }
            Err(error) => {
                tracing::warn!(%card, origin = origin.label(), "capture failed: {error}");
                let _ = self.outcomes.send(Outcome::Failed { card, error });
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
        let bytes = FrameEncoder::new().encode(&capture.frame, ImageFormat::Png)?;
        let thumbnail = Thumbnail::from_frame(&capture.frame, THUMBNAIL_MAX_EDGE).ok();
        let capture_id = self.remember(&capture);

        self.cache.insert(card, Cached { bytes });

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

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{
        Error, RegionSelector, Result as CoreResult, SelectionCapabilities, SelectionOutcome,
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
