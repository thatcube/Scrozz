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
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{Receiver, Sender, SyncSender, channel, sync_channel},
    },
    thread::JoinHandle,
    time::SystemTime,
};

use scrozz_annotate::Document;
use scrozz_core::{Capture, CaptureRequest, CaptureTarget, CursorMode, Error as CoreError};
#[cfg(not(target_os = "macos"))]
use scrozz_export::SystemClipboard;
use scrozz_export::{Encoder, FrameEncoder, ImageFormat};
use scrozz_shell::ByteSource;
use scrozz_store::{CaptureId, History, NewCapture, SqliteStore, Store};

use crate::{
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
    /// Put a card's capture on the clipboard.
    Copy(CardId),
    /// Write a card's capture to the configured folder.
    Save(CardId),
    /// Upload a card through the configured provider.
    Upload(CardId),
    /// Persist a capture's pin state.
    Pin {
        /// The card.
        card: CardId,
        /// The new state.
        pinned: bool,
    },
    /// Return cached or history-rehydrated PNG bytes to a native callback.
    Bytes {
        /// The card.
        card: CardId,
        /// One-shot result channel.
        reply: SyncSender<Result<Vec<u8>, String>>,
    },
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
        /// Whether successful completion retires the visible card.
        dismiss: bool,
    },
    /// Capture history accepted a pin-state change.
    PinUpdated {
        /// Which card.
        card: CardId,
        /// The persisted state.
        pinned: bool,
        /// What happened.
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
    pub fn start() -> CliResult<Self> {
        let (jobs, job_rx) = channel();
        let (outcome_tx, outcomes) = channel();

        let worker = std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || Worker::new(outcome_tx).run(&job_rx))
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

    /// A delayed PNG producer backed by the worker cache.
    ///
    /// Native file promises call this only when a drop target asks for bytes.
    #[must_use]
    pub fn byte_source(&self, card: CardId) -> ByteSource {
        let jobs = self.jobs.clone();
        Arc::new(move || {
            let (reply, result) = sync_channel(1);
            jobs.send(Job::Bytes { card, reply }).map_err(|_| {
                CoreError::TargetGone(format!(
                    "{card} cannot provide drag bytes because the capture worker stopped"
                ))
            })?;
            result
                .recv()
                .map_err(|_| {
                    CoreError::TargetGone(format!(
                        "{card} cannot provide drag bytes because the capture worker stopped"
                    ))
                })?
                .map_err(CoreError::TargetGone)
        })
    }

    /// Materializes a capture into a self-contained delayed byte source.
    ///
    /// AppKit may fulfill a promised file after the visible drag has ended and
    /// the card cache has been released. Leasing the bytes before starting the
    /// native session keeps that delayed callback independent of pipeline state.
    pub fn lease_bytes(&self, card: CardId) -> Result<ByteSource, CoreError> {
        Ok(leased_byte_source(self.byte_source(card)()?))
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

fn leased_byte_source(bytes: Vec<u8>) -> ByteSource {
    let bytes = Arc::new(bytes);
    Arc::new(move || Ok(bytes.as_ref().clone()))
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the worker remembers about a card it produced.
struct Cached {
    bytes: Arc<Vec<u8>>,
}

struct Worker {
    outcomes: Sender<Outcome>,
    store: Option<SqliteStore>,
    cache: HashMap<CardId, Cached>,
    history_ids: HashMap<CardId, CaptureId>,
    saved: HashMap<CardId, PathBuf>,
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

        Self {
            outcomes,
            store,
            cache: HashMap::new(),
            history_ids: HashMap::new(),
            saved: HashMap::new(),
        }
    }

    fn run(mut self, jobs: &Receiver<Job>) {
        while let Ok(job) = jobs.recv() {
            match job {
                Job::Capture { kind, card } => self.capture(kind, card),
                Job::Copy(card) => self.copy(card),
                Job::Save(card) => self.save(card),
                Job::Upload(card) => self.upload(card),
                Job::Pin { card, pinned } => self.pin(card, pinned),
                Job::Bytes { card, reply } => {
                    let result = self
                        .png_bytes(card, "drag")
                        .map_err(|error| error.to_string());
                    let _ = reply.send(result);
                }
                Job::Release(card) => self.release(card),
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
                bytes: Arc::new(bytes),
            },
        );
        if let Some(capture_id) = capture_id.clone() {
            self.history_ids.insert(card, capture_id);
        }

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
        let result = self.png_bytes(card, "copy").and_then(|bytes| {
            #[cfg(target_os = "macos")]
            {
                scrozz_shell::macos::clipboard::write_png(&bytes)?;
                Ok("copied to the clipboard as PNG and TIFF".to_owned())
            }
            #[cfg(not(target_os = "macos"))]
            {
                // The round trip through PNG is deliberate — see the module
                // docs — and also supports history-rehydrated cards.
                let frame = scrozz_export::decode(&bytes)?;
                SystemClipboard::new().write_image_reporting(&frame)?;
                Ok("copied to the clipboard".to_owned())
            }
        });
        self.answer(card, result, true);
    }

    fn save(&mut self, card: CardId) {
        let result = self.save_with(card, crate::output::export_default);
        self.answer(card, result, true);
    }

    fn save_with(
        &mut self,
        card: CardId,
        export: impl FnOnce(&[u8]) -> CliResult<PathBuf>,
    ) -> CliResult<String> {
        if let Some(path) = self.saved.get(&card) {
            Ok(format!("already saved to {}", path.display()))
        } else {
            self.png_bytes(card, "save").and_then(|bytes| {
                let path = export(&bytes)?;
                self.saved.insert(card, path.clone());
                Ok(format!("saved to {}", path.display()))
            })
        }
    }

    fn upload(&mut self, card: CardId) {
        let result = self.png_bytes(card, "upload").and_then(|_| {
            Err(CliError::not_implemented(
                "uploading a capture",
                "an S3Uploader configured for the GUI capture pipeline",
            ))
        });
        self.answer(card, result, false);
    }

    fn pin(&mut self, card: CardId, pinned: bool) {
        let result = (|| {
            let capture = self
                .history_ids
                .get(&card)
                .cloned()
                .ok_or_else(|| CliError::usage(format!("{card} is not present in history")))?;
            let store = self
                .store
                .as_mut()
                .ok_or_else(|| CliError::usage("capture history is unavailable"))?;
            store.set_pinned(&capture, pinned)?;
            Ok(if pinned {
                "pinned in capture history".to_owned()
            } else {
                "unpinned in capture history".to_owned()
            })
        })();
        let message = match result {
            Ok(detail) => Outcome::PinUpdated {
                card,
                pinned,
                detail,
            },
            Err(error) => Outcome::Refused { card, error },
        };
        let _ = self.outcomes.send(message);
    }

    fn png_bytes(&mut self, card: CardId, verb: &str) -> CliResult<Vec<u8>> {
        if let Some(cached) = self.cache.get(&card) {
            return Ok(cached.bytes.as_ref().clone());
        }

        let capture = self
            .history_ids
            .get(&card)
            .cloned()
            .ok_or_else(|| CliError::usage(format!("{card} has no capture to {verb}")))?;
        let store = self
            .store
            .as_mut()
            .ok_or_else(|| CliError::usage("capture history is unavailable"))?;
        let document = store
            .document(&capture)?
            .and_then(scrozz_store::DocumentState::complete)
            .ok_or_else(|| {
                CliError::usage(format!(
                    "{card} cannot be restored because its source image was evicted"
                ))
            })?;
        let bytes = FrameEncoder::new().encode(&document.source.frame, ImageFormat::Png)?;
        self.cache.insert(
            card,
            Cached {
                bytes: Arc::new(bytes.clone()),
            },
        );
        Ok(bytes)
    }

    fn release(&mut self, card: CardId) {
        self.cache.remove(&card);
    }

    fn answer(&self, card: CardId, result: CliResult<String>, dismiss: bool) {
        let message = match result {
            Ok(detail) => Outcome::Done {
                card,
                detail,
                dismiss,
            },
            Err(error) => Outcome::Refused { card, error },
        };
        let _ = self.outcomes.send(message);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use scrozz_store::test_support::{sample_document, scratch_dir};

    use super::*;

    fn worker(store: Option<SqliteStore>) -> (Worker, Receiver<Outcome>) {
        let (outcomes, replies) = channel();
        (
            Worker {
                outcomes,
                store,
                cache: HashMap::new(),
                history_ids: HashMap::new(),
                saved: HashMap::new(),
            },
            replies,
        )
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
    fn byte_source_reaches_the_worker_and_reports_a_missing_card() {
        let pipeline = Pipeline::start().expect("the worker should start");
        let error =
            pipeline.byte_source(CardId(404))().expect_err("an unknown card has no PNG bytes");
        assert!(error.to_string().contains("404"), "{error}");
    }

    #[test]
    fn released_bytes_are_rehydrated_from_history_on_demand() {
        let dir = scratch_dir("pipeline-rehydrate");
        let mut store = SqliteStore::open(dir.path()).expect("history opens");
        let capture = store
            .insert(NewCapture::new(&sample_document(4, 3, 7, 0)))
            .expect("capture enters history");
        let (mut worker, _) = worker(Some(store));
        let card = CardId(12);
        worker.history_ids.insert(card, capture);

        let first = worker.png_bytes(card, "drag").expect("history rehydrates");
        assert!(first.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(worker.cache.contains_key(&card));

        worker.release(card);
        assert!(!worker.cache.contains_key(&card));
        let second = worker
            .png_bytes(card, "copy")
            .expect("a restored card rehydrates again");
        assert_eq!(second, first);
    }

    #[test]
    fn a_materialized_byte_lease_survives_cache_release() {
        let (mut worker, _) = worker(None);
        let card = CardId(13);
        let bytes = b"\x89PNG\r\n\x1a\nleased".to_vec();
        worker.cache.insert(
            card,
            Cached {
                bytes: Arc::new(bytes.clone()),
            },
        );

        let leased = leased_byte_source(worker.png_bytes(card, "drag").expect("lease bytes"));
        worker.release(card);

        assert!(!worker.cache.contains_key(&card));
        assert_eq!(leased().expect("delayed promise still owns bytes"), bytes);
    }

    #[test]
    fn saving_the_same_card_exports_exactly_once() {
        let (mut worker, _) = worker(None);
        let card = CardId(8);
        let bytes = b"\x89PNG\r\n\x1a\ncapture".to_vec();
        worker.cache.insert(
            card,
            Cached {
                bytes: Arc::new(bytes.clone()),
            },
        );
        let calls = Cell::new(0);
        let path = PathBuf::from("/tmp/scrozz-save-once.png");

        let first = worker
            .save_with(card, |given| {
                calls.set(calls.get() + 1);
                assert_eq!(given, bytes);
                Ok(path.clone())
            })
            .expect("first save");
        let second = worker
            .save_with(card, |_| {
                calls.set(calls.get() + 1);
                Ok(PathBuf::from("/tmp/duplicate.png"))
            })
            .expect("second save");

        assert_eq!(calls.get(), 1);
        assert!(first.contains("saved to"));
        assert!(second.contains("already saved"));
        assert_eq!(worker.saved.get(&card), Some(&path));
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
