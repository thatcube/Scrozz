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
//! So the main thread does one thing on a hotkey: post a [`Job`]. Capture and
//! mutation work runs on the capture worker; thumbnail-heavy history reads run
//! on an independent, coalescing reader so opening a large library cannot delay
//! the shutter. Both report [`Outcome`] values the main thread picks up on its
//! next tick.
//!
//! # What the worker owns
//!
//! The capture worker owns the writing store handle and encoded bytes of cards
//! still on screen. Both deliberately. `SqliteStore` holds a
//! `rusqlite::Connection`, so keeping mutations on one thread means there is
//! exactly one GUI writer and no lock discipline to get wrong. The history
//! reader has its own read connection, relying on SQLite WAL snapshots just as a
//! forwarded CLI invocation does. And keeping the *encoded* bytes rather than
//! the `Frame` is what makes
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

use scrozz_annotate::{Document, Renderer, SkiaRenderer};
use scrozz_core::{
    Capture, CaptureRequest, CaptureTarget, CursorMode, Error as CoreError, LogicalPoint,
    LogicalRect, Provenance, ScaleFactor,
};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, SystemClipboard};
use scrozz_shell::{DragPayload, DragPreview, byte_source};
use scrozz_store::{
    CaptureId, CaptureRecord, DocumentState, History, NewCapture, RetentionPolicy, SearchQuery,
    SqliteStore, Store,
};
use scrozz_ui::history::{HistoryEntry, HistoryPage, HistoryThumbnail};

use crate::{
    fault::{CliError, CliResult},
    gui::{
        action::CaptureKind,
        card::{Card, CardId, THUMBNAIL_MAX_EDGE, Thumbnail},
    },
    platform,
};

/// Geometry the native drag backend needs from the source window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DragGeometry {
    /// Card rectangle in source-window points or history preview in screen points.
    pub rect: LogicalRect,
    /// Pointer position in the same coordinate space as `rect`.
    pub pointer: LogicalPoint,
}

/// What one prepared drag belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragSubject {
    /// A live card in the overlay.
    Card(CardId),
    /// A durable capture in the history window.
    History(CaptureId),
}

/// History operation names shared by logs and view-model feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryOperation {
    /// Read one filtered page.
    Query,
    /// Return a stored capture to the live pile.
    Restore,
    /// Load the editable annotation document.
    OpenEditor,
    /// Put the rendered capture on the clipboard.
    Copy,
    /// Export the rendered capture.
    Save,
    /// Prepare a promised-file drag.
    Drag,
    /// Change retention protection.
    Pin,
    /// Permanently remove the capture.
    Delete,
    /// Apply the image retention policy.
    Retention,
}

impl HistoryOperation {
    /// Stable present-tense label for diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Query => "load history",
            Self::Restore => "restore capture",
            Self::OpenEditor => "open editor",
            Self::Copy => "copy capture",
            Self::Save => "save capture",
            Self::Drag => "drag capture",
            Self::Pin => "change pinned state",
            Self::Delete => "delete capture",
            Self::Retention => "enforce retention",
        }
    }
}

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
    /// Prepare a live card for native drag-out.
    Drag {
        /// Card being dragged.
        card: CardId,
        /// Geometry reported by the overlay.
        geometry: DragGeometry,
    },
    /// Restore a stored capture into a new live card.
    Restore {
        /// Durable capture.
        capture: CaptureId,
        /// Session-local card allocated before posting.
        card: CardId,
    },
    /// Load a stored document for annotation.
    OpenEditor(CaptureId),
    /// Load the history document behind a live card for annotation.
    OpenCard(CardId),
    /// Copy a stored document.
    CopyHistory(CaptureId),
    /// Save a stored document.
    SaveHistory(CaptureId),
    /// Prepare a stored document for drag-out.
    DragHistory {
        /// Durable capture.
        capture: CaptureId,
        /// Geometry reported by the history viewport.
        geometry: DragGeometry,
    },
    /// Change a stored capture's pinned state.
    SetPinned {
        /// Durable capture.
        capture: CaptureId,
        /// New state.
        pinned: bool,
    },
    /// Permanently delete a stored capture.
    Delete(CaptureId),
    /// Replace the live source-image retention policy and enforce it now.
    ///
    /// The worker also applies this policy after every future capture.
    EnforceRetention(RetentionPolicy),
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
    /// A stored capture was rebuilt as a live card.
    Restored(Box<Card>),
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
    /// A filtered history page is ready.
    HistoryLoaded {
        /// Query generation this answers.
        request: u64,
        /// Rows, count, and application choices.
        page: HistoryPage,
    },
    /// A history operation failed.
    HistoryFailed {
        /// Query generation for query failures; absent for mutations.
        request: Option<u64>,
        /// Operation that failed.
        operation: HistoryOperation,
        /// Capture involved, when there was one.
        capture: Option<CaptureId>,
        /// The explicit failure.
        error: CliError,
    },
    /// A history mutation or export completed.
    HistoryDone {
        /// Operation that completed.
        operation: HistoryOperation,
        /// Capture involved, when there was one.
        capture: Option<CaptureId>,
        /// New pinned state for a pin operation.
        pinned: Option<bool>,
        /// User-facing result.
        detail: String,
    },
    /// An editable stored document is ready for the editor host.
    EditorReady {
        /// Durable identity.
        capture: CaptureId,
        /// Full document, including untouched pixels and editable annotations.
        document: Box<Document>,
    },
    /// A promised-file drag is ready to begin on the UI thread.
    DragReady {
        /// Live card or history capture.
        subject: DragSubject,
        /// Promise and preview bytes.
        payload: DragPayload,
        /// Source geometry.
        geometry: DragGeometry,
    },
    /// A drag payload could not be prepared, so any held live source may resume.
    DragFailed {
        /// Live card or history capture.
        subject: DragSubject,
        /// Explicit preparation failure.
        error: CliError,
    },
}

/// A handle to the capture and history threads.
pub struct Pipeline {
    jobs: Sender<Job>,
    history_queries: Sender<HistoryQuery>,
    outcomes: Receiver<Outcome>,
    worker: Option<JoinHandle<()>>,
    history_worker: Option<JoinHandle<()>>,
    next_card: u64,
}

#[derive(Debug)]
enum HistoryQuery {
    Load { request: u64, query: SearchQuery },
    Stop,
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
        Self::start_with_retention(RetentionPolicy::default())
    }

    /// Starts the worker with the source-image policy loaded from settings.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Core`] under the same conditions as [`Self::start`].
    pub fn start_with_retention(retention_policy: RetentionPolicy) -> CliResult<Self> {
        let (jobs, job_rx) = channel();
        let (history_queries, history_rx) = channel();
        let (outcome_tx, outcomes) = channel();
        let history_outcomes = outcome_tx.clone();

        let worker = std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || Worker::new(outcome_tx, retention_policy).run(&job_rx))
            .map_err(|err| {
                CliError::Core(CoreError::Platform(format!(
                    "could not start the capture worker: {err}"
                )))
            })?;
        let history_worker = match std::thread::Builder::new()
            .name("scrozz-history".to_owned())
            .spawn(move || HistoryReader::new(history_outcomes).run(&history_rx))
        {
            Ok(worker) => worker,
            Err(err) => {
                let _ = jobs.send(Job::Stop);
                let _ = worker.join();
                return Err(CliError::Core(CoreError::Platform(format!(
                    "could not start the history worker: {err}"
                ))));
            }
        };

        Ok(Self {
            jobs,
            history_queries,
            outcomes,
            worker: Some(worker),
            history_worker: Some(history_worker),
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

    /// Replaces the live policy and asks the worker to enforce it immediately.
    ///
    /// Completion or failure arrives as a retention history outcome.
    #[must_use]
    pub fn set_retention_policy(&self, policy: RetentionPolicy) -> bool {
        self.post(Job::EnforceRetention(policy))
    }

    /// Posts a coalescible history read on a worker independent of capture.
    pub fn query_history(&self, request: u64, query: SearchQuery) -> bool {
        self.history_queries
            .send(HistoryQuery::Load { request, query })
            .is_ok()
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
        let _ = self.history_queries.send(HistoryQuery::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.history_worker.take() {
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
    bytes: Arc<Vec<u8>>,
    stem: String,
    capture: Option<CaptureId>,
}

struct Worker {
    outcomes: Sender<Outcome>,
    store: Option<SqliteStore>,
    cache: HashMap<CardId, Cached>,
    retention_policy: RetentionPolicy,
}

impl Worker {
    fn new(outcomes: Sender<Outcome>, retention_policy: RetentionPolicy) -> Self {
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
            retention_policy,
        }
    }

    fn run(mut self, jobs: &Receiver<Job>) {
        if let Err(error) = self.enforce_current_retention() {
            tracing::warn!("initial source-image retention could not run: {error}");
        }
        while let Ok(job) = jobs.recv() {
            match job {
                Job::Capture { kind, card } => self.capture(kind, card),
                Job::Copy(card) => self.copy(card),
                Job::Save(card) => self.save(card),
                Job::Drag { card, geometry } => self.drag_card(card, geometry),
                Job::Restore { capture, card } => self.restore(capture, card),
                Job::OpenEditor(capture) => self.open_editor(capture),
                Job::OpenCard(card) => self.open_card(card),
                Job::CopyHistory(capture) => self.copy_history(capture),
                Job::SaveHistory(capture) => self.save_history(capture),
                Job::DragHistory { capture, geometry } => {
                    self.drag_history(capture, geometry);
                }
                Job::SetPinned { capture, pinned } => self.set_pinned(capture, pinned),
                Job::Delete(capture) => self.delete(capture),
                Job::EnforceRetention(policy) => self.set_retention_policy(policy),
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
        };
        self.cache.insert(
            card,
            Cached {
                bytes: Arc::new(bytes),
                stem: "Scrozz capture".to_owned(),
                capture: built.capture_id.clone(),
            },
        );

        Ok(built)
    }

    /// Persists a capture, or explains in the log why it was not.
    fn remember(&mut self, capture: &Capture) -> Option<CaptureId> {
        let policy = self.retention_policy.clone();
        let store = self.store.as_mut()?;
        let document = Document::new(capture.clone());
        match store.insert(NewCapture::of_kind(
            &document,
            scrozz_store::MediaKind::Screenshot,
        )) {
            Ok(id) => {
                if let Err(err) = store.enforce_retention(&policy) {
                    tracing::warn!("capture was stored but retention could not run: {err}");
                }
                Some(id)
            }
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
            .and_then(|cached| Ok(scrozz_export::decode(cached.bytes.as_slice())?))
            .and_then(|frame| {
                SystemClipboard::new().write_image_reporting(&frame)?;
                Ok("copied to the clipboard".to_owned())
            });
        self.answer(card, result);
    }

    fn save(&mut self, card: CardId) {
        let result = self.cached(card, "save").and_then(|cached| {
            let path = crate::output::export_default(cached.bytes.as_slice())?;
            Ok(format!("saved to {}", path.display()))
        });
        self.answer(card, result);
    }

    fn drag_card(&mut self, card: CardId, geometry: DragGeometry) {
        let result = self
            .cached(card, "drag")
            .and_then(|cached| drag_payload(&cached.stem, Arc::clone(&cached.bytes), geometry));
        match result {
            Ok(payload) => {
                let _ = self.outcomes.send(Outcome::DragReady {
                    subject: DragSubject::Card(card),
                    payload,
                    geometry,
                });
            }
            Err(error) => {
                let _ = self.outcomes.send(Outcome::DragFailed {
                    subject: DragSubject::Card(card),
                    error,
                });
            }
        }
    }

    fn restore(&mut self, capture: CaptureId, card: CardId) {
        let result = self.render_stored(&capture).map(|rendered| {
            let kind = capture_kind(rendered.record.provenance);
            let thumbnail = Thumbnail::from_frame(&rendered.frame, THUMBNAIL_MAX_EDGE).ok();
            let built = Card {
                id: card,
                capture_id: Some(capture.clone()),
                kind,
                source_width: rendered.frame.width(),
                source_height: rendered.frame.height(),
                scale: rendered.frame.scale.get(),
                thumbnail,
                written: Vec::new(),
                taken_at: rendered.record.created_at.to_system_time(),
            };
            self.cache.insert(
                card,
                Cached {
                    bytes: Arc::clone(&rendered.bytes),
                    stem: stem_for(&rendered.record),
                    capture: Some(capture.clone()),
                },
            );
            built
        });

        match result {
            Ok(card) => {
                let _ = self.outcomes.send(Outcome::Restored(Box::new(card)));
                self.history_done(
                    HistoryOperation::Restore,
                    Some(capture),
                    None,
                    "restored to the capture stack".to_owned(),
                );
            }
            Err(error) => {
                self.history_failed(HistoryOperation::Restore, Some(capture), error);
            }
        }
    }

    fn open_editor(&mut self, capture: CaptureId) {
        let result = self.load_document(&capture);
        match result {
            Ok(document) => {
                let _ = self.outcomes.send(Outcome::EditorReady {
                    capture,
                    document: Box::new(document),
                });
            }
            Err(error) => {
                self.history_failed(HistoryOperation::OpenEditor, Some(capture), error);
            }
        }
    }

    fn open_card(&mut self, card: CardId) {
        let capture = match self.cached(card, "open for editing") {
            Ok(cached) => cached.capture.clone().ok_or_else(|| {
                CliError::Core(CoreError::Storage(format!(
                        "{card} was captured while history was unavailable, so it has no stored document"
                    )))
            }),
            Err(error) => Err(error),
        };
        match capture {
            Ok(capture) => self.open_editor(capture),
            Err(error) => {
                let _ = self.outcomes.send(Outcome::Refused { card, error });
            }
        }
    }

    fn copy_history(&mut self, capture: CaptureId) {
        let result = self
            .render_stored(&capture)
            .and_then(|rendered| Ok(scrozz_export::decode(rendered.bytes.as_slice())?))
            .and_then(|frame| {
                SystemClipboard::new().write_image_reporting(&frame)?;
                Ok("copied to the clipboard".to_owned())
            });
        self.answer_history(HistoryOperation::Copy, capture, result);
    }

    fn save_history(&mut self, capture: CaptureId) {
        let result = self.render_stored(&capture).and_then(|rendered| {
            let path = crate::output::export_default(rendered.bytes.as_slice())?;
            Ok(format!("saved to {}", path.display()))
        });
        self.answer_history(HistoryOperation::Save, capture, result);
    }

    fn drag_history(&mut self, capture: CaptureId, geometry: DragGeometry) {
        let result = self.render_stored(&capture).and_then(|rendered| {
            drag_payload(
                &stem_for(&rendered.record),
                Arc::clone(&rendered.bytes),
                geometry,
            )
        });
        match result {
            Ok(payload) => {
                let _ = self.outcomes.send(Outcome::DragReady {
                    subject: DragSubject::History(capture),
                    payload,
                    geometry,
                });
            }
            Err(error) => {
                self.history_failed(HistoryOperation::Drag, Some(capture), error);
            }
        }
    }

    fn set_pinned(&mut self, capture: CaptureId, pinned: bool) {
        let result = self
            .history_store()
            .and_then(|store| Ok(store.set_pinned(&capture, pinned)?))
            .map(|()| {
                if pinned {
                    "capture pinned".to_owned()
                } else {
                    "capture unpinned".to_owned()
                }
            });
        match result {
            Ok(detail) => {
                self.history_done(HistoryOperation::Pin, Some(capture), Some(pinned), detail)
            }
            Err(error) => self.history_failed(HistoryOperation::Pin, Some(capture), error),
        }
    }

    fn delete(&mut self, capture: CaptureId) {
        let result = self
            .history_store()
            .and_then(|store| Ok(store.delete(&capture)?))
            .and_then(|deleted| {
                if deleted {
                    Ok("capture deleted".to_owned())
                } else {
                    Err(history_not_found(&capture))
                }
            });
        self.answer_history(HistoryOperation::Delete, capture, result);
    }

    fn set_retention_policy(&mut self, policy: RetentionPolicy) {
        self.retention_policy = policy;
        let result = self.enforce_current_retention();
        match result {
            Ok(detail) => self.history_done(HistoryOperation::Retention, None, None, detail),
            Err(error) => self.history_failed(HistoryOperation::Retention, None, error),
        }
    }

    fn enforce_current_retention(&mut self) -> CliResult<String> {
        let policy = self.retention_policy.clone();
        self.history_store()
            .and_then(|store| Ok(store.enforce_retention(&policy)?))
            .map(|()| "source-image retention enforced".to_owned())
    }

    fn history_store(&mut self) -> CliResult<&mut SqliteStore> {
        self.store.as_mut().ok_or_else(|| {
            CliError::Core(CoreError::Storage(
                "capture history is unavailable in this session".to_owned(),
            ))
        })
    }

    fn load_document(&mut self, capture: &CaptureId) -> CliResult<Document> {
        match self.history_store()?.document(capture)? {
            Some(DocumentState::Complete(document)) => Ok(document),
            Some(DocumentState::ImageEvicted(_)) => Err(history_image_evicted(capture)),
            None => Err(history_not_found(capture)),
        }
    }

    fn render_stored(&mut self, capture: &CaptureId) -> CliResult<RenderedStored> {
        let record = self
            .history_store()?
            .record(capture)?
            .ok_or_else(|| history_not_found(capture))?;
        let document = self.load_document(capture)?;
        let frame = SkiaRenderer::new().render(&document)?;
        let bytes = Arc::new(FrameEncoder::new().encode(&frame, ImageFormat::Png)?);
        Ok(RenderedStored {
            record,
            frame,
            bytes,
        })
    }

    fn answer_history(
        &self,
        operation: HistoryOperation,
        capture: CaptureId,
        result: CliResult<String>,
    ) {
        match result {
            Ok(detail) => self.history_done(operation, Some(capture), None, detail),
            Err(error) => self.history_failed(operation, Some(capture), error),
        }
    }

    fn history_done(
        &self,
        operation: HistoryOperation,
        capture: Option<CaptureId>,
        pinned: Option<bool>,
        detail: String,
    ) {
        let _ = self.outcomes.send(Outcome::HistoryDone {
            operation,
            capture,
            pinned,
            detail,
        });
    }

    fn history_failed(
        &self,
        operation: HistoryOperation,
        capture: Option<CaptureId>,
        error: CliError,
    ) {
        let _ = self.outcomes.send(Outcome::HistoryFailed {
            request: None,
            operation,
            capture,
            error,
        });
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

struct HistoryReader {
    outcomes: Sender<Outcome>,
    store: Option<SqliteStore>,
    open_default: bool,
}

impl HistoryReader {
    fn new(outcomes: Sender<Outcome>) -> Self {
        Self {
            outcomes,
            store: None,
            open_default: true,
        }
    }

    fn run(mut self, queries: &Receiver<HistoryQuery>) {
        'worker: while let Ok(message) = queries.recv() {
            let (mut request, mut query) = match message {
                HistoryQuery::Load { request, query } => (request, query),
                HistoryQuery::Stop => break,
            };
            while let Ok(message) = queries.try_recv() {
                match message {
                    HistoryQuery::Load {
                        request: newer_request,
                        query: newer_query,
                    } => {
                        request = newer_request;
                        query = newer_query;
                    }
                    HistoryQuery::Stop => break 'worker,
                }
            }
            self.query(request, &query);
        }
        tracing::debug!("history worker stopped");
    }

    fn query(&mut self, request: u64, query: &SearchQuery) {
        let result = self
            .history_store()
            .and_then(|store| load_history_page(store, query));
        let outcome = match result {
            Ok(page) => Outcome::HistoryLoaded { request, page },
            Err(error) => Outcome::HistoryFailed {
                request: Some(request),
                operation: HistoryOperation::Query,
                capture: None,
                error,
            },
        };
        let _ = self.outcomes.send(outcome);
    }

    fn history_store(&mut self) -> CliResult<&mut SqliteStore> {
        if self.store.is_none() && self.open_default {
            self.store = Some(SqliteStore::open_default()?);
        }
        self.store.as_mut().ok_or_else(|| {
            CliError::Core(CoreError::Storage(
                "capture history is unavailable in this session".to_owned(),
            ))
        })
    }
}

fn load_history_page(store: &mut SqliteStore, query: &SearchQuery) -> CliResult<HistoryPage> {
    let mut fallback = None;
    for _ in 0..3 {
        let total_before = store.count_matching(query)?;
        let records = store.search(query)?;
        let apps = store.apps()?;
        let mut entries = Vec::with_capacity(records.len());
        let mut missing_document = false;
        for record in records {
            if let Some(entry) = history_entry(store, record)? {
                entries.push(entry);
            } else {
                missing_document = true;
            }
        }
        let total_after = store.count_matching(query)?;
        let page = HistoryPage {
            entries,
            total: total_after,
            apps,
            offset: query.page.offset,
            limit: query.page.limit,
        };
        if !missing_document && total_before == total_after {
            return Ok(page);
        }
        fallback = Some(page);
    }
    fallback.ok_or_else(|| {
        CliError::Core(CoreError::Storage(
            "capture history changed too quickly to read".to_owned(),
        ))
    })
}

struct RenderedStored {
    record: CaptureRecord,
    frame: scrozz_core::Frame,
    bytes: Arc<Vec<u8>>,
}

fn history_entry(
    store: &mut SqliteStore,
    record: CaptureRecord,
) -> CliResult<Option<HistoryEntry>> {
    let Some(document) = store.document(&record.id)? else {
        return Ok(None);
    };
    let image_present = matches!(document, DocumentState::Complete(_));
    let thumbnail = match document {
        DocumentState::Complete(document) => {
            let preview = history_thumbnail(&document, record.frame.scale)?;
            let rgba = scrozz_export::to_straight_rgba8(&preview)?;
            Some(
                HistoryThumbnail::from_rgba(rgba.width, rgba.height, rgba.data).ok_or_else(
                    || {
                        CliError::Core(CoreError::Codec(format!(
                            "capture {} produced malformed thumbnail pixels",
                            record.id.0
                        )))
                    },
                )?,
            )
        }
        DocumentState::ImageEvicted(_) => None,
    };
    let width = dimension(record.frame.size.width);
    let height = dimension(record.frame.size.height);
    Ok(Some(HistoryEntry {
        id: record.id,
        created_at: record.created_at,
        media_kind: record.media_kind,
        pinned: record.pinned,
        app_name: record.app_name,
        window_title: record.window_title,
        width,
        height,
        scale: record.frame.scale.get(),
        image_present,
        annotation_count: record.annotation_count,
        ocr_text: record.ocr_text,
        thumbnail,
    }))
}

fn history_thumbnail(
    document: &Document,
    source_scale: ScaleFactor,
) -> CliResult<scrozz_core::Frame> {
    let logical = document.logical_size();
    let padding = document
        .beautification()
        .map_or(0.0, |beautification| beautification.padding.max(0.0) * 2.0);
    let longest = (logical.width + padding).max(logical.height + padding);
    if !longest.is_finite() || longest <= 0.0 {
        return Err(CliError::Core(CoreError::InvalidRequest(
            "capture history thumbnail has invalid geometry".to_owned(),
        )));
    }

    let renderer = SkiaRenderer::new();
    let mut scale = source_scale
        .get()
        .min(f64::from(THUMBNAIL_MAX_EDGE) / longest);
    let mut preview = renderer.render_at(document, ScaleFactor::new(scale))?;
    let rendered_longest = preview.width().max(preview.height());
    if rendered_longest > THUMBNAIL_MAX_EDGE {
        scale *= f64::from(THUMBNAIL_MAX_EDGE) / f64::from(rendered_longest);
        preview = renderer.render_at(document, ScaleFactor::new(scale))?;
    }
    Ok(preview)
}

fn dimension(value: f64) -> u32 {
    value.round().clamp(0.0, f64::from(u32::MAX)) as u32
}

fn capture_kind(provenance: Provenance) -> CaptureKind {
    match provenance {
        Provenance::Window => CaptureKind::Window,
        Provenance::Region | Provenance::Stitched => CaptureKind::Region,
        Provenance::Display | Provenance::AllDisplays => CaptureKind::Fullscreen,
    }
}

fn stem_for(record: &CaptureRecord) -> String {
    record
        .window_title
        .as_deref()
        .or(record.app_name.as_deref())
        .unwrap_or("Scrozz capture")
        .to_owned()
}

fn drag_payload(stem: &str, bytes: Arc<Vec<u8>>, geometry: DragGeometry) -> CliResult<DragPayload> {
    let promised = Arc::clone(&bytes);
    let source = byte_source(move || Ok(promised.as_ref().clone()));
    let preview = DragPreview::from_png(bytes.as_ref().clone(), geometry.rect.size)?;
    Ok(DragPayload::png_capture(stem, source).with_preview(preview))
}

fn history_not_found(capture: &CaptureId) -> CliError {
    CliError::Core(CoreError::Storage(format!(
        "capture {:?} was not found in history",
        capture.0
    )))
}

fn history_image_evicted(capture: &CaptureId) -> CliError {
    CliError::Core(CoreError::Storage(format!(
        "capture {:?} is still in history, but its source image was evicted by the retention policy",
        capture.0
    )))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use scrozz_store::test_support::{
        ScratchDir, richly_annotated_document, sample_document, sample_frame, scratch_dir,
    };

    use super::*;

    fn worker_with_store(label: &str) -> (ScratchDir, Worker, Receiver<Outcome>) {
        let dir = scratch_dir(label);
        let store = SqliteStore::open_ephemeral(dir.path()).expect("open isolated history");
        let (outcomes, receiver) = channel();
        (
            dir,
            Worker {
                outcomes,
                store: Some(store),
                cache: HashMap::new(),
                retention_policy: RetentionPolicy::default(),
            },
            receiver,
        )
    }

    fn received(receiver: &Receiver<Outcome>) -> Outcome {
        receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("worker outcome")
    }

    fn test_pipeline() -> Pipeline {
        Pipeline::start_with_retention(RetentionPolicy {
            max_image_bytes: u64::MAX,
            max_image_age: scrozz_store::RetentionWindow::Forever,
        })
        .expect("the worker should start")
    }

    #[test]
    fn a_pipeline_hands_out_distinct_card_identities() {
        let mut pipeline = test_pipeline();
        let first = pipeline.allocate();
        let second = pipeline.allocate();
        assert_ne!(first, second);
        assert_eq!(first, CardId(1));
        assert_eq!(second, CardId(2));
    }

    #[test]
    fn a_live_policy_update_evicts_existing_images_and_governs_future_captures() {
        let (_dir, mut worker, outcomes) = worker_with_store("pipeline-retention-update");
        let first = worker
            .remember(&sample_document(8, 8, 1, 0).source)
            .expect("store existing capture");
        assert!(
            worker
                .store
                .as_mut()
                .unwrap()
                .image(&first)
                .unwrap()
                .is_some()
        );

        let policy = RetentionPolicy {
            max_image_bytes: 0,
            max_image_age: scrozz_store::RetentionWindow::Forever,
        };
        worker.set_retention_policy(policy.clone());
        assert_eq!(worker.retention_policy, policy);
        assert!(
            worker
                .store
                .as_mut()
                .unwrap()
                .image(&first)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            received(&outcomes),
            Outcome::HistoryDone {
                operation: HistoryOperation::Retention,
                ..
            }
        ));

        let future = worker
            .remember(&sample_document(8, 8, 2, 0).source)
            .expect("store future capture");
        let store = worker.store.as_mut().unwrap();
        assert!(store.image(&future).unwrap().is_none());
        assert!(store.record(&future).unwrap().is_some());
    }

    #[test]
    fn the_startup_policy_is_enforced_before_the_first_job() {
        let (dir, mut worker, _outcomes) = worker_with_store("pipeline-startup-retention");
        let capture = worker
            .store
            .as_mut()
            .unwrap()
            .insert(NewCapture::new(&sample_document(8, 8, 3, 0)))
            .expect("store capture before worker startup");
        worker.retention_policy = RetentionPolicy {
            max_image_bytes: 0,
            max_image_age: scrozz_store::RetentionWindow::Forever,
        };

        let (jobs, receiver) = channel();
        jobs.send(Job::Stop).unwrap();
        worker.run(&receiver);

        let mut reopened = SqliteStore::open(dir.path()).expect("reopen history");
        assert!(reopened.image(&capture).unwrap().is_none());
        assert!(reopened.record(&capture).unwrap().is_some());
    }

    #[test]
    fn polling_an_idle_pipeline_does_not_block() {
        let pipeline = test_pipeline();
        assert!(pipeline.poll().is_none());
    }

    #[test]
    fn queued_history_reads_coalesce_to_the_newest_generation() {
        let dir = scratch_dir("history-coalescing");
        let store = SqliteStore::open_ephemeral(dir.path()).expect("history store");
        let (outcomes, receiver) = channel();
        let (queries, query_receiver) = channel();
        queries
            .send(HistoryQuery::Load {
                request: 1,
                query: SearchQuery::all().text("obsolete"),
            })
            .expect("first query");
        queries
            .send(HistoryQuery::Load {
                request: 2,
                query: SearchQuery::all(),
            })
            .expect("newest query");
        let worker = std::thread::spawn(move || {
            HistoryReader {
                outcomes,
                store: Some(store),
                open_default: false,
            }
            .run(&query_receiver);
        });

        let Outcome::HistoryLoaded { request, .. } = received(&receiver) else {
            panic!("expected a history page");
        };
        assert_eq!(request, 2);
        queries.send(HistoryQuery::Stop).expect("stop history");
        worker.join().expect("history worker");
        drop(dir);
    }

    #[test]
    fn a_pipeline_stops_cleanly_and_twice_is_harmless() {
        // Drop also stops it, so the second call must be a no-op rather than a
        // join on an already-joined handle.
        let mut pipeline = test_pipeline();
        pipeline.stop();
        pipeline.stop();
    }

    #[test]
    fn copying_a_card_that_was_never_captured_is_refused_not_ignored() {
        let pipeline = test_pipeline();
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
        let pipeline = test_pipeline();
        assert!(pipeline.post(Job::Save(CardId(7))));

        match wait_for(&pipeline) {
            Some(Outcome::Refused { card, .. }) => assert_eq!(card, CardId(7)),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn releasing_an_unknown_card_is_harmless() {
        let pipeline = test_pipeline();
        assert!(pipeline.post(Job::Release(CardId(1))));
        assert!(pipeline.post(Job::Release(CardId(1))));
    }

    #[test]
    fn a_history_read_returns_filtered_entries_counts_apps_and_previews() {
        let (_dir, mut worker, _receiver) = worker_with_store("worker-query");
        let video = sample_document(64, 32, 1, 2);
        let screenshot = sample_document(40, 20, 2, 0);
        let video_id = worker
            .store
            .as_mut()
            .expect("store")
            .insert(
                NewCapture::of_kind(&video, scrozz_store::MediaKind::Video)
                    .from_app("Figma")
                    .titled("Needle review")
                    .with_ocr("needle in recognised text")
                    .taken_at(scrozz_store::Timestamp(2_000)),
            )
            .expect("insert video");
        worker
            .store
            .as_mut()
            .expect("store")
            .insert(
                NewCapture::new(&screenshot)
                    .from_app("Preview")
                    .titled("Other capture")
                    .taken_at(scrozz_store::Timestamp(1_000)),
            )
            .expect("insert screenshot");

        let query = SearchQuery::all()
            .text("needle")
            .kind(scrozz_store::MediaKind::Video)
            .paged(scrozz_store::Page::new(1, 0));
        let page =
            load_history_page(worker.store.as_mut().expect("store"), &query).expect("history page");
        assert_eq!(page.total, 1);
        assert_eq!(page.offset, 0);
        assert_eq!(page.limit, 1);
        assert_eq!(page.apps, ["Figma", "Preview"]);
        assert_eq!(page.entries.len(), 1);
        let entry = &page.entries[0];
        assert_eq!(entry.id, video_id);
        assert_eq!(entry.media_kind, scrozz_store::MediaKind::Video);
        assert_eq!(entry.annotation_count, 2);
        assert!(entry.image_present);
        assert!(entry.thumbnail.is_some());
    }

    #[test]
    fn history_thumbnails_bound_the_longest_edge_for_scrolling_captures() {
        for (width, height) in [(8, 4_096), (4_096, 8)] {
            let document = sample_document(width, height, 9, 0);
            let preview =
                history_thumbnail(&document, ScaleFactor::IDENTITY).expect("render thumbnail");
            assert!(
                preview.width().max(preview.height()) <= THUMBNAIL_MAX_EDGE,
                "{}x{} exceeded the longest-edge cap",
                preview.width(),
                preview.height()
            );
        }
    }

    #[test]
    fn restore_and_editor_load_the_complete_stored_document() {
        let (_dir, mut worker, receiver) = worker_with_store("worker-restore");
        let document = richly_annotated_document(42);
        let id = worker
            .store
            .as_mut()
            .expect("store")
            .insert(NewCapture::new(&document).titled("Editable"))
            .expect("insert");

        worker.open_editor(id.clone());
        let Outcome::EditorReady {
            capture,
            document: loaded,
        } = received(&receiver)
        else {
            panic!("expected an editable document");
        };
        assert_eq!(capture, id);
        assert_eq!(loaded.len(), document.len());
        assert_eq!(loaded.data(), document.data());

        worker.restore(id.clone(), CardId(9));
        let Outcome::Restored(card) = received(&receiver) else {
            panic!("expected a restored card");
        };
        assert_eq!(card.id, CardId(9));
        assert_eq!(card.capture_id.as_ref(), Some(&id));
        assert_eq!(card.source_px(), (128, 112));
        assert!(worker.cache.contains_key(&CardId(9)));
        assert!(matches!(
            received(&receiver),
            Outcome::HistoryDone {
                operation: HistoryOperation::Restore,
                ..
            }
        ));
    }

    #[test]
    fn pin_and_delete_mutate_the_worker_store() {
        let (_dir, mut worker, receiver) = worker_with_store("worker-mutations");
        let document = sample_document(16, 8, 3, 1);
        let id = worker
            .store
            .as_mut()
            .expect("store")
            .insert(NewCapture::new(&document))
            .expect("insert");

        worker.set_pinned(id.clone(), true);
        assert!(matches!(
            received(&receiver),
            Outcome::HistoryDone {
                operation: HistoryOperation::Pin,
                pinned: Some(true),
                ..
            }
        ));
        assert!(
            worker
                .store
                .as_ref()
                .expect("store")
                .record(&id)
                .expect("read")
                .expect("record")
                .pinned
        );

        worker.delete(id.clone());
        assert!(matches!(
            received(&receiver),
            Outcome::HistoryDone {
                operation: HistoryOperation::Delete,
                ..
            }
        ));
        assert!(
            worker
                .store
                .as_ref()
                .expect("store")
                .record(&id)
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn preparing_a_native_drag_keeps_the_live_cards_cache() {
        let (_dir, mut worker, receiver) = worker_with_store("worker-drag");
        let card = Card::placeholder(CardId(4), CaptureKind::Fullscreen);
        let bytes = FrameEncoder::new()
            .encode(&sample_frame(16, 8, 7), ImageFormat::Png)
            .expect("encode");
        worker.cache.insert(
            card.id,
            Cached {
                bytes: Arc::new(bytes),
                stem: "Drag me".to_owned(),
                capture: None,
            },
        );
        let geometry = DragGeometry {
            rect: LogicalRect::new(
                LogicalPoint::new(10.0, 20.0),
                scrozz_core::LogicalSize::new(240.0, 160.0),
            ),
            pointer: LogicalPoint::new(80.0, 60.0),
        };

        worker.drag_card(card.id, geometry);
        assert!(matches!(
            received(&receiver),
            Outcome::DragReady {
                subject: DragSubject::Card(CardId(4)),
                ..
            }
        ));
        assert!(
            worker.cache.contains_key(&card.id),
            "the cache must survive until the native drop outcome is known"
        );
    }

    #[test]
    fn preparing_a_broken_drag_reports_failure_without_discarding_the_cache() {
        let (_dir, mut worker, receiver) = worker_with_store("worker-broken-drag");
        let card = Card::placeholder(CardId(5), CaptureKind::Fullscreen);
        worker.cache.insert(
            card.id,
            Cached {
                bytes: Arc::new(Vec::new()),
                stem: "Broken".to_owned(),
                capture: None,
            },
        );
        let geometry = DragGeometry {
            rect: LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                scrozz_core::LogicalSize::new(100.0, 80.0),
            ),
            pointer: LogicalPoint::new(20.0, 20.0),
        };
        worker.drag_card(card.id, geometry);
        worker.drag_card(card.id, geometry);
        assert!(matches!(
            received(&receiver),
            Outcome::DragFailed {
                subject: DragSubject::Card(CardId(5)),
                ..
            }
        ));
        assert!(worker.cache.contains_key(&card.id));
    }

    #[test]
    fn unavailable_history_is_an_explicit_worker_failure() {
        let (outcomes, receiver) = channel();
        let mut reader = HistoryReader {
            outcomes,
            store: None,
            open_default: false,
        };

        reader.query(6, &SearchQuery::all());
        let Outcome::HistoryFailed {
            request,
            operation,
            error,
            ..
        } = received(&receiver)
        else {
            panic!("expected an explicit history failure");
        };
        assert_eq!(request, Some(6));
        assert_eq!(operation, HistoryOperation::Query);
        assert!(error.to_string().contains("unavailable"), "{error}");
    }

    #[test]
    fn a_gui_worker_and_cli_connection_can_use_history_concurrently() {
        let dir = scratch_dir("worker-cli-concurrency");
        let root = dir.path().to_path_buf();
        let store = SqliteStore::open(&root).expect("open GUI store");
        let (outcomes, _receiver) = channel();
        let mut worker = Worker {
            outcomes,
            store: Some(store),
            cache: HashMap::new(),
            retention_policy: RetentionPolicy::default(),
        };
        let gate = Arc::new(Barrier::new(2));
        let writer_gate = Arc::clone(&gate);
        let writer_root = root.clone();
        let writer = std::thread::spawn(move || {
            let mut cli = SqliteStore::open(writer_root).expect("open CLI store");
            writer_gate.wait();
            for seed in 0..12 {
                let document = sample_document(8, 8, seed, 0);
                cli.insert(
                    NewCapture::new(&document)
                        .from_app("CLI")
                        .titled(format!("capture {seed}")),
                )
                .expect("concurrent CLI insert");
            }
        });

        gate.wait();
        for _ in 0..12 {
            load_history_page(worker.store.as_mut().expect("store"), &SearchQuery::all())
                .expect("GUI query while CLI writes");
        }
        writer.join().expect("CLI writer");
        let final_page =
            load_history_page(worker.store.as_mut().expect("store"), &SearchQuery::all())
                .expect("final GUI query");
        assert_eq!(final_page.total, 12);
        assert_eq!(final_page.entries.len(), 12);
        drop(worker);
        drop(dir);
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
