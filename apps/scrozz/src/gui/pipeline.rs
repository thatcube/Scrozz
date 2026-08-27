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
    /// Restore a card's full document and open it in the editor.
    Open(CardId),
    /// Persist non-destructive editor metadata.
    Persist {
        /// Which editor/card initiated the write.
        card: CardId,
        /// Monotonic snapshot revision assigned by the application.
        revision: u64,
        /// Stable history identity, retained even if the card is dismissed.
        capture: CaptureId,
        /// Mutable document data only.
        data: DocumentData,
    },
    /// Render and deliver an editor snapshot.
    Export {
        /// Which editor/card initiated the export.
        card: CardId,
        /// Stable history identity used to restore immutable source pixels.
        capture: CaptureId,
        /// Concrete destination selected by the application.
        destination: Destination,
        /// Compact snapshot of current edits.
        data: DocumentData,
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
    /// A persisted document is ready for an ordinary editor viewport.
    EditorReady {
        /// Which card requested it.
        card: CardId,
        /// Stable history identity used for subsequent metadata saves.
        capture: CaptureId,
        /// Human-readable editor title.
        title: String,
        /// Full non-destructive document.
        document: Box<Document>,
    },
    /// An editor restore failed before a viewport could open.
    EditorRefused {
        /// Which card requested the editor.
        card: CardId,
        /// Why its document could not be restored.
        error: CliError,
    },
    /// An editor export completed.
    EditorExported {
        /// Which editor requested it.
        card: CardId,
        /// What was delivered.
        detail: String,
    },
    /// An editor export failed.
    EditorExportRefused {
        /// Which editor requested it.
        card: CardId,
        /// Why.
        error: CliError,
    },
    /// Editor metadata was persisted.
    EditorPersisted {
        /// Which editor initiated the write.
        card: CardId,
        /// Which snapshot finished.
        revision: u64,
    },
    /// Editor metadata could not be persisted.
    EditorPersistRefused {
        /// Which editor initiated the write.
        card: CardId,
        /// Which snapshot failed.
        revision: u64,
        /// Why.
        error: CliError,
    },
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
    bytes: Vec<u8>,
    capture_id: Option<CaptureId>,
    title: String,
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
                Job::Open(card) => self.open(card),
                Job::Persist {
                    card,
                    revision,
                    capture,
                    data,
                } => self.persist(card, revision, &capture, &data),
                Job::Export {
                    card,
                    capture,
                    destination,
                    data,
                } => self.export(card, &capture, destination, data),
                Job::Release(card) => {
                    self.cache.remove(&card);
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

        let built = Card {
            id: card,
            capture_id: capture_id.clone(),
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
        };
        self.cache.insert(
            card,
            Cached {
                bytes,
                capture_id,
                title: built.file_name(),
            },
        );

        Ok(built)
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

    fn open(&mut self, card: CardId) {
        let result = self.open_document(card);
        let message = match result {
            Ok((capture, title, document)) => Outcome::EditorReady {
                card,
                capture,
                title,
                document: Box::new(document),
            },
            Err(error) => Outcome::EditorRefused { card, error },
        };
        let _ = self.outcomes.send(message);
    }

    fn open_document(&mut self, card: CardId) -> CliResult<(CaptureId, String, Document)> {
        let cached = self.cached(card, "edit")?;
        let capture = cached.capture_id.clone().ok_or_else(|| {
            CliError::Core(CoreError::Storage(
                "this capture was shown but could not be added to history, so its editable \
                 document is unavailable"
                    .to_owned(),
            ))
        })?;
        let title = cached.title.clone();
        let store = self.store.as_mut().ok_or_else(|| {
            CliError::Core(CoreError::Storage(
                "capture history is unavailable, so the editable document cannot be restored"
                    .to_owned(),
            ))
        })?;
        let state = store.document(&capture)?.ok_or_else(|| {
            CliError::Core(CoreError::Storage(format!(
                "capture {} is no longer in history",
                capture.0
            )))
        })?;
        let document = state.complete().ok_or_else(|| {
            CliError::Core(CoreError::Storage(
                "the source pixels were evicted from history; its edit metadata remains, but it \
                 can no longer be rendered"
                    .to_owned(),
            ))
        })?;
        Ok((capture, title, document))
    }

    fn persist(&mut self, card: CardId, revision: u64, capture: &CaptureId, data: &DocumentData) {
        let result = self
            .store
            .as_mut()
            .ok_or_else(|| {
                CliError::Core(CoreError::Storage(
                    "capture history is unavailable; changes were not saved".to_owned(),
                ))
            })
            .and_then(|store| {
                store.save_edits(capture, data)?;
                Ok("saved changes".to_owned())
            });
        let outcome = match result {
            Ok(_) => Outcome::EditorPersisted { card, revision },
            Err(error) => Outcome::EditorPersistRefused {
                card,
                revision,
                error,
            },
        };
        let _ = self.outcomes.send(outcome);
    }

    fn export(
        &mut self,
        card: CardId,
        capture: &CaptureId,
        destination: Destination,
        data: DocumentData,
    ) {
        let result = self
            .document_for_export(capture, data)
            .and_then(|document| Ok(SkiaRenderer.render(&document)?))
            .and_then(|frame| {
                let outcome = crate::output::export_frame_auto(&frame, &destination)?;
                match destination {
                    Destination::Clipboard => {
                        Ok("copied the edited image to the clipboard".to_owned())
                    }
                    Destination::Folder(_) => {
                        let path = outcome.path.ok_or_else(|| {
                            CliError::Core(CoreError::Storage(
                                "the folder exporter succeeded without returning a path".to_owned(),
                            ))
                        })?;
                        Ok(format!("saved the edited image to {}", path.display()))
                    }
                    Destination::S3 { .. } => unreachable!("editor does not expose uploads"),
                }
            });
        let outcome = match result {
            Ok(detail) => Outcome::EditorExported { card, detail },
            Err(error) => Outcome::EditorExportRefused { card, error },
        };
        let _ = self.outcomes.send(outcome);
    }

    fn document_for_export(
        &mut self,
        capture: &CaptureId,
        data: DocumentData,
    ) -> CliResult<Document> {
        let store = self.store.as_mut().ok_or_else(|| {
            CliError::Core(CoreError::Storage(
                "capture history is unavailable, so the edited image cannot be exported".to_owned(),
            ))
        })?;
        let state = store.document(capture)?.ok_or_else(|| {
            CliError::Core(CoreError::Storage(format!(
                "capture {} is no longer in history",
                capture.0
            )))
        })?;
        let document = state.complete().ok_or_else(|| {
            CliError::Core(CoreError::Storage(
                "the source pixels were evicted from history; its edits remain, but it can no \
                 longer be exported"
                    .to_owned(),
            ))
        })?;
        Ok(Document::from_data(document.source, data)?)
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
mod editor_tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use scrozz_annotate::{Background, Beautification, BeautificationPreset};
    use scrozz_core::{
        ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize, PhysicalSize, Provenance,
        ScaleFactor,
    };

    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "scrozz-pipeline-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn capture() -> Capture {
        let width = 80_u32;
        let height = 48_u32;
        Capture {
            frame: Frame {
                data: [74, 102, 213, 255]
                    .into_iter()
                    .cycle()
                    .take(width as usize * height as usize * 4)
                    .collect(),
                size: PhysicalSize::new(f64::from(width), f64::from(height)),
                stride: width as usize * 4,
                format: scrozz_core::PixelFormat::Rgba8,
                color_space: ColorSpace::DisplayP3,
                scale: ScaleFactor::new(2.0),
            },
            provenance: Provenance::Region,
            target: CaptureTarget::Region(LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(40.0, 24.0),
            )),
        }
    }

    fn stored_worker(label: &str) -> (Worker, Receiver<Outcome>, CaptureId, PathBuf) {
        let root = scratch(label);
        let mut store = SqliteStore::open_ephemeral(&root).expect("ephemeral store");
        let document = Document::new(capture());
        let capture_id = store
            .insert(NewCapture::new(&document))
            .expect("insert capture");
        let (outcome_tx, outcome_rx) = channel();
        let card = CardId(7);
        let mut cache = HashMap::new();
        cache.insert(
            card,
            Cached {
                bytes: Vec::new(),
                capture_id: Some(capture_id.clone()),
                title: "Scrozz fixture.png".to_owned(),
            },
        );
        (
            Worker {
                outcomes: outcome_tx,
                store: Some(store),
                cache,
            },
            outcome_rx,
            capture_id,
            root,
        )
    }

    #[test]
    fn open_restores_the_full_non_destructive_document() {
        let (mut worker, outcomes, capture_id, root) = stored_worker("open");
        worker.open(CardId(7));

        let Outcome::EditorReady {
            card,
            capture: restored_id,
            title,
            document,
        } = outcomes.recv().expect("editor outcome")
        else {
            panic!("expected editor-ready outcome");
        };
        assert_eq!(card, CardId(7));
        assert_eq!(restored_id, capture_id);
        assert_eq!(title, "Scrozz fixture.png");
        assert_eq!(document.source.frame.data, capture().frame.data);
        fs::remove_dir_all(root).expect("remove scratch store");
    }

    #[test]
    fn persist_writes_only_document_metadata_back_to_history() {
        let (mut worker, outcomes, capture_id, root) = stored_worker("persist");
        let source = capture().frame.data;
        let mut data = Document::new(capture()).data();
        data.beautification = Some(Beautification::preset(BeautificationPreset::Editorial));
        worker.persist(CardId(7), 23, &capture_id, &data);
        assert!(matches!(
            outcomes.recv().expect("save outcome"),
            Outcome::EditorPersisted {
                card: CardId(7),
                revision: 23
            }
        ));

        let restored = worker
            .store
            .as_mut()
            .expect("store")
            .document(&capture_id)
            .expect("read document")
            .expect("stored document")
            .complete()
            .expect("source retained");
        assert_eq!(restored.source.frame.data, source);
        assert_eq!(restored.beautification(), data.beautification.as_ref());
        fs::remove_dir_all(root).expect("remove scratch store");
    }

    #[test]
    fn editor_export_uses_destination_policy_profiles_and_retina_names() {
        let (mut worker, outcome_rx, capture_id, root) = stored_worker("export");
        let export_root = root.join("exports");
        fs::create_dir_all(&export_root).expect("create export folder");
        let mut data = Document::new(capture()).data();
        let mut framing = Beautification::preset(BeautificationPreset::Clean);
        framing.background = Background::Transparent;
        data.beautification = Some(framing);

        worker.export(
            CardId(7),
            &capture_id,
            Destination::Folder(export_root.clone()),
            data,
        );
        assert!(matches!(
            outcome_rx.recv().expect("export outcome"),
            Outcome::EditorExported {
                card: CardId(7),
                ..
            }
        ));
        let paths: Vec<PathBuf> = fs::read_dir(&export_root)
            .expect("list export")
            .map(|entry| entry.expect("directory entry").path())
            .collect();
        assert_eq!(paths.len(), 1);
        let path = &paths[0];
        assert!(
            path.file_name()
                .expect("file name")
                .to_string_lossy()
                .contains("@2x"),
            "retina suffix: {}",
            path.display()
        );
        let bytes = fs::read(path).expect("read export");
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(
            bytes.windows(b"iCCP".len()).any(|window| window == b"iCCP"),
            "the sRGB working space should carry an ICC profile chunk"
        );
        assert_eq!(
            scrozz_export::decode(&bytes)
                .expect("decode exported image")
                .color_space,
            ColorSpace::Srgb
        );
        fs::remove_dir_all(root).expect("remove export folder");
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
