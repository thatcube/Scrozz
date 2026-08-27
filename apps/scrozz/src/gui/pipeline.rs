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
    time::SystemTime,
};

use scrozz_annotate::{Document, DocumentData, Renderer, SkiaRenderer};
use scrozz_core::{Capture, CaptureRequest, CaptureTarget, CursorMode, Error as CoreError};
use scrozz_export::{Destination, Encoder, FrameEncoder, ImageFormat, SystemClipboard};
use scrozz_store::{CaptureId, History, NewCapture, SqliteStore};
use scrozz_ui::EditorDestination;

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
    /// Forget a card's cached bytes. The card itself is the surface's business.
    Release(CardId),
    /// Load a card's durable editable document.
    LoadEditor(CardId),
    /// Persist an editor snapshot through the existing history document.
    PersistEdits {
        /// Durable capture identity. Editor windows outlive cards.
        capture: CaptureId,
        /// Monotonic editor revision used to reject stale acknowledgements.
        revision: u64,
        /// The complete editable snapshot; source pixels remain in history.
        data: DocumentData,
    },
    /// Persist and export one exact editor snapshot as a single worker barrier.
    ExportEdits {
        /// Durable capture identity. Editor windows outlive cards.
        capture: CaptureId,
        /// Monotonic editor revision acknowledged by this export.
        revision: u64,
        /// The complete editable snapshot to persist and render.
        data: DocumentData,
        /// User-selected delivery destination.
        destination: EditorDestination,
    },
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
    /// A complete document is ready for an annotation viewport.
    EditorLoaded {
        /// Durable capture identity used for every later editor operation.
        capture: CaptureId,
        /// Source pixels and the latest non-destructive edits.
        document: Box<Document>,
    },
    /// A revision is durable in both the annotation sidecar and history index.
    EditsSaved {
        /// Durable capture identity.
        capture: CaptureId,
        /// Acknowledged editor revision.
        revision: u64,
    },
    /// A revision could not be made durable and remains retryable.
    EditsSaveFailed {
        /// Durable capture identity.
        capture: CaptureId,
        /// Failed editor revision.
        revision: u64,
        /// Store or queue failure.
        error: CliError,
    },
    /// An exact editor revision was persisted and delivered.
    EditsExported {
        /// Durable capture identity.
        capture: CaptureId,
        /// Acknowledged editor revision.
        revision: u64,
        /// Destination that completed.
        destination: EditorDestination,
        /// User-visible terminal result.
        detail: String,
    },
    /// An editor export failed before delivery completed.
    EditsExportFailed {
        /// Durable capture identity.
        capture: CaptureId,
        /// Revision that remains retryable.
        revision: u64,
        /// Destination that failed.
        destination: EditorDestination,
        /// Store, render, or delivery failure.
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

        Self {
            outcomes,
            store,
            cache: HashMap::new(),
        }
    }

    fn run(mut self, jobs: &Receiver<Job>) {
        while let Ok(job) = jobs.recv() {
            match job {
                Job::Capture { kind, card } => self.capture(kind, card),
                Job::Copy(card) => self.copy(card),
                Job::Save(card) => self.save(card),
                Job::Release(card) => {
                    if self
                        .cache
                        .get(&card)
                        .is_some_and(|cached| cached.capture_id.is_none())
                    {
                        self.cache.remove(&card);
                    }
                }
                Job::LoadEditor(card) => self.load_editor(card),
                Job::PersistEdits {
                    capture,
                    revision,
                    data,
                } => self.persist_edits(capture, revision, &data),
                Job::ExportEdits {
                    capture,
                    revision,
                    data,
                    destination,
                } => self.export_edits(capture, revision, data, destination),
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
        let thumbnail = Thumbnail::from_frame(&capture.frame, THUMBNAIL_MAX_EDGE).ok();
        let capture_id = self.remember(&capture);

        self.cache.insert(
            card,
            Cached {
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
        let result = self
            .render_card(card, "copy")
            .and_then(|bytes| Ok(scrozz_export::decode(&bytes)?))
            .and_then(|frame| {
                SystemClipboard::new().write_image_reporting(&frame)?;
                Ok("copied to the clipboard".to_owned())
            });
        self.answer(card, result);
    }

    fn save(&mut self, card: CardId) {
        let result = self.render_card(card, "save").and_then(|bytes| {
            let path = crate::output::export_default(&bytes)?;
            Ok(format!("saved to {}", path.display()))
        });
        self.answer(card, result);
    }

    fn load_editor(&mut self, card: CardId) {
        let result = self
            .capture_id(card, "annotate")
            .and_then(|capture_id| {
                let store = self.store.as_mut().ok_or_else(|| {
                    CliError::usage(
                        "history is unavailable, so this capture has no editable document",
                    )
                })?;
                let state = store
                    .document(&capture_id)?
                    .ok_or_else(|| CliError::usage(format!("{card} is no longer in history")))?;
                let document = state.complete().ok_or_else(|| {
                    CliError::usage(format!(
                        "{card}'s source image was evicted; its annotations remain in history but cannot be edited without the pixels"
                    ))
                })?;
                Ok((capture_id, document))
            });

        match result {
            Ok((capture, document)) => {
                let _ = self.outcomes.send(Outcome::EditorLoaded {
                    capture,
                    document: Box::new(document),
                });
            }
            Err(error) => self.answer_error(card, error),
        }
    }

    fn persist_edits(&mut self, capture: CaptureId, revision: u64, data: &DocumentData) {
        let result = self
            .store
            .as_mut()
            .ok_or_else(|| CliError::usage("history is unavailable, so edits cannot be persisted"))
            .and_then(|store| {
                store.save_edits(&capture, data)?;
                Ok(())
            });
        let outcome = match result {
            Ok(()) => Outcome::EditsSaved { capture, revision },
            Err(error) => Outcome::EditsSaveFailed {
                capture,
                revision,
                error,
            },
        };
        let _ = self.outcomes.send(outcome);
    }

    fn export_edits(
        &mut self,
        capture: CaptureId,
        revision: u64,
        data: DocumentData,
        destination: EditorDestination,
    ) {
        let result = self
            .document_for_export(&capture, data)
            .and_then(|document| Ok(SkiaRenderer::new().render(&document)?))
            .and_then(|frame| {
                let target = match destination {
                    EditorDestination::Clipboard => Destination::Clipboard,
                    EditorDestination::DefaultFolder => {
                        Destination::Folder(crate::output::default_directory())
                    }
                };
                let outcome = crate::output::export_frame_auto(&frame, &target)?;
                match destination {
                    EditorDestination::Clipboard => {
                        Ok("Copied the edited image.".to_owned())
                    }
                    EditorDestination::DefaultFolder => {
                        let path = outcome.path.ok_or_else(|| {
                            CliError::Core(CoreError::Storage(
                                "the folder exporter succeeded without returning a path".to_owned(),
                            ))
                        })?;
                        Ok(format!("Saved the edited image to {}.", path.display()))
                    }
                }
            });
        let outcome = match result {
            Ok(detail) => Outcome::EditsExported {
                capture,
                revision,
                destination,
                detail,
            },
            Err(error) => Outcome::EditsExportFailed {
                capture,
                revision,
                destination,
                error,
            },
        };
        let _ = self.outcomes.send(outcome);
    }

    fn document_for_export(
        &mut self,
        capture: &CaptureId,
        data: DocumentData,
    ) -> CliResult<Document> {
        let store = self.store.as_mut().ok_or_else(|| {
            CliError::usage("history is unavailable, so the edited image cannot be exported")
        })?;
        store.save_edits(capture, &data)?;
        let state = store
            .document(capture)?
            .ok_or_else(|| CliError::usage(format!("{} is no longer in history", capture.0)))?;
        let document = state.complete().ok_or_else(|| {
            CliError::usage(format!(
                "{}'s source image was evicted; its edits remain but cannot be exported",
                capture.0
            ))
        })?;
        Ok(Document::from_data(document.source, data)?)
    }

    fn capture_id(&self, card: CardId, verb: &str) -> CliResult<CaptureId> {
        self.cached(card, verb)?
            .capture_id
            .clone()
            .ok_or_else(|| CliError::usage(format!("{card} is not available in history to {verb}")))
    }

    fn cached(&self, card: CardId, verb: &str) -> CliResult<&Cached> {
        self.cache
            .get(&card)
            .ok_or_else(|| CliError::usage(format!("{card} has no capture to {verb}")))
    }

    fn render_card(&mut self, card: CardId, verb: &str) -> CliResult<Vec<u8>> {
        let capture = self.capture_id(card, verb)?;
        self.render_stored(&capture)
    }

    fn render_stored(&mut self, capture: &CaptureId) -> CliResult<Vec<u8>> {
        let store = self.store.as_mut().ok_or_else(|| {
            CliError::usage("history is unavailable, so the edited capture cannot be exported")
        })?;
        let state = store
            .document(capture)?
            .ok_or_else(|| CliError::usage(format!("{} is no longer in history", capture.0)))?;
        let document = state.complete().ok_or_else(|| {
            CliError::usage(format!(
                "{}'s source image was evicted; its edits remain but cannot be exported",
                capture.0
            ))
        })?;
        let frame = SkiaRenderer::new().render(&document)?;
        Ok(FrameEncoder::new().encode(&frame, ImageFormat::Png)?)
    }

    fn answer(&self, card: CardId, result: CliResult<String>) {
        let message = match result {
            Ok(detail) => Outcome::Done { card, detail },
            Err(error) => Outcome::Refused { card, error },
        };
        let _ = self.outcomes.send(message);
    }

    fn answer_error(&self, card: CardId, error: CliError) {
        let _ = self.outcomes.send(Outcome::Refused { card, error });
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
    fn annotating_a_card_that_was_never_captured_is_refused_too() {
        let pipeline = Pipeline::start().expect("the worker should start");
        assert!(pipeline.post(Job::LoadEditor(CardId(8))));

        match wait_for(&pipeline) {
            Some(Outcome::Refused { card, error }) => {
                assert_eq!(card, CardId(8));
                assert!(error.to_string().contains("annotate"), "{error}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn editor_load_and_save_use_the_workers_existing_store() {
        use scrozz_store::test_support::{sample_document, scratch_dir};

        let scratch = scratch_dir("pipeline-editor");
        let (outcome_tx, outcome_rx) = channel();
        let mut store = SqliteStore::open_ephemeral(scratch.path()).expect("store");
        let original = sample_document(16, 12, 4, 1);
        let capture_id = store
            .insert(NewCapture::new(&original))
            .expect("capture inserted");
        let card = CardId(23);
        let mut worker = Worker {
            outcomes: outcome_tx,
            store: Some(store),
            cache: HashMap::from([(
                card,
                Cached {
                    capture_id: Some(capture_id.clone()),
                },
            )]),
        };

        worker.load_editor(card);
        let loaded = match outcome_rx.recv().expect("load outcome") {
            Outcome::EditorLoaded { capture, document } => {
                assert_eq!(capture, capture_id);
                *document
            }
            other => panic!("expected an editor document, got {other:?}"),
        };
        assert_eq!(loaded.data(), original.data());

        let mut changed = loaded.data();
        changed.canvas.auto_expand = true;
        worker.persist_edits(capture_id.clone(), 7, &changed);
        match outcome_rx.recv().expect("save outcome") {
            Outcome::EditsSaved { capture, revision } => {
                assert_eq!(capture, capture_id);
                assert_eq!(revision, 7);
            }
            other => panic!("expected a correlated save acknowledgement, got {other:?}"),
        }
        let reloaded = worker
            .store
            .as_mut()
            .expect("store retained")
            .document(&capture_id)
            .expect("read")
            .expect("record")
            .complete()
            .expect("source retained");
        assert_eq!(reloaded.data(), changed);
    }

    #[test]
    fn rendered_export_never_contains_pixels_hidden_by_persisted_redaction() {
        use scrozz_annotate::{Annotation, RedactStyle, Style};
        use scrozz_core::{
            ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize, PhysicalSize, PixelFormat,
            Provenance, ScaleFactor,
        };
        use scrozz_store::test_support::scratch_dir;

        const SECRET: [u8; 4] = [237, 19, 211, 255];
        let scratch = scratch_dir("pipeline-redaction-export");
        let mut store = SqliteStore::open_ephemeral(scratch.path()).expect("store");
        let frame = Frame {
            data: SECRET.repeat(16 * 12),
            size: PhysicalSize::new(16.0, 12.0),
            stride: 16 * 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::IDENTITY,
        };
        let capture = Capture {
            frame,
            provenance: Provenance::Region,
            target: CaptureTarget::Region(LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(16.0, 12.0),
            )),
        };
        let mut document = Document::new(capture);
        let capture_id = store
            .insert(NewCapture::new(&document))
            .expect("capture inserted");
        document.add(
            Annotation::Redact {
                area: document.logical_bounds(),
                style: RedactStyle::Solid,
            },
            Style::redaction(),
        );
        store
            .save_edits(&capture_id, &document.data())
            .expect("redaction persisted");

        let (outcome_tx, _outcome_rx) = channel();
        let mut worker = Worker {
            outcomes: outcome_tx,
            store: Some(store),
            cache: HashMap::new(),
        };
        let encoded = worker
            .render_stored(&capture_id)
            .expect("render persisted document");
        let exported = scrozz_export::decode(&encoded).expect("decode rendered export");
        assert!(
            exported.data.chunks_exact(4).all(|pixel| pixel != SECRET),
            "an external export exposed a source pixel hidden by redaction"
        );

        let retained = worker
            .store
            .as_mut()
            .unwrap()
            .document(&capture_id)
            .unwrap()
            .unwrap()
            .complete()
            .unwrap();
        assert_eq!(
            &retained.source.frame.data[..4],
            &SECRET,
            "D14 keeps immutable source pixels only inside the editable document"
        );
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
