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

use scrozz_annotate::{
    AnalysisCancellation, Document, DocumentData, Renderer, SkiaRenderer, SmartFrameAnalysis,
    analyze_smart_frame,
};
use scrozz_core::{CaptureRequest, CaptureTarget, CursorMode, Error as CoreError};
use scrozz_export::{Destination, Encoder, FrameEncoder, ImageFormat, SystemClipboard};
use scrozz_store::{CaptureId, History, NewCapture, SqliteStore};

use crate::{
    fault::{CliError, CliResult},
    gui::{
        action::CaptureKind,
        card::{Card, CardId, THUMBNAIL_MAX_EDGE, Thumbnail},
    },
    platform,
    settings::AfterCapturePolicy,
};

const MAX_ANALYSIS_CACHE_ENTRIES: usize = 32;

/// One isolated After Capture consumer result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerResult {
    /// Stable consumer name.
    pub consumer: &'static str,
    /// Success detail or actionable failure.
    pub result: std::result::Result<String, String>,
}

/// Stable After Capture execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterCaptureConsumer {
    /// System clipboard.
    Copy,
    /// Configured/default folder.
    Save,
    /// Configured upload provider.
    Upload,
    /// Quick Access overlay.
    Overlay,
    /// Editable document viewport.
    OpenEditor,
    /// Floating pinned image.
    Pin,
}

impl AfterCaptureConsumer {
    const ORDERED: [Self; 6] = [
        Self::Copy,
        Self::Save,
        Self::Upload,
        Self::Overlay,
        Self::OpenEditor,
        Self::Pin,
    ];

    const fn slug(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Save => "save",
            Self::Upload => "upload",
            Self::Overlay => "overlay",
            Self::OpenEditor => "open-editor",
            Self::Pin => "pin",
        }
    }
}

/// Capture plus the main-thread work selected by After Capture settings.
#[derive(Debug)]
pub struct ReadyCapture {
    /// Card containing the exact derived revision preview.
    pub card: Box<Card>,
    /// Whether to show Quick Access.
    pub show_overlay: bool,
    /// Whether to open the editable document.
    pub open_editor: bool,
    /// Results from worker-owned consumers.
    pub consumers: Vec<ConsumerResult>,
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
    /// Analyse one immutable editor revision without blocking capture work.
    AnalyzeSmartFrame {
        /// Which editor requested analysis.
        card: CardId,
        /// Stable history identity used to restore source pixels.
        capture: CaptureId,
        /// Revision echoed back for stale-result rejection.
        revision: u64,
        /// Current annotations, with framing removed.
        data: DocumentData,
        /// Cooperative cancellation handle.
        cancellation: AnalysisCancellation,
    },
    /// Replace the GUI-only After Capture policy after a live settings update.
    ConfigureAfterCapture(AfterCapturePolicy),
    /// Forget a card's cached bytes. The card itself is the surface's business.
    Release(CardId),
    /// Finish, so the thread can be joined.
    Stop,
}

/// What the capture thread produced.
#[derive(Debug)]
pub enum Outcome {
    /// A capture succeeded and is ready to show.
    Ready(ReadyCapture),
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
    /// Smart Frame analysis completed or failed.
    SmartFrameAnalyzed {
        /// Which editor requested it.
        card: CardId,
        /// Revision the result belongs to.
        revision: u64,
        /// Fully resolved settings or an actionable failure.
        result: Box<std::result::Result<SmartFrameAnalysis, String>>,
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
        Self::start_with_policy(AfterCapturePolicy::default())
    }

    /// Starts the worker with a persisted GUI-only After Capture policy.
    pub fn start_with_policy(after_capture: AfterCapturePolicy) -> CliResult<Self> {
        let (jobs, job_rx) = channel();
        let (outcome_tx, outcomes) = channel();

        let worker = std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || Worker::new(outcome_tx, after_capture).run(&job_rx))
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
    after_capture: AfterCapturePolicy,
    analysis_cache: Arc<Mutex<HashMap<AnalysisCacheKey, SmartFrameAnalysis>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AnalysisCacheKey {
    capture: String,
    document_fingerprint: u64,
    algorithm_version: u16,
}

impl Worker {
    fn new(outcomes: Sender<Outcome>, after_capture: AfterCapturePolicy) -> Self {
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
            after_capture,
            analysis_cache: Arc::new(Mutex::new(HashMap::new())),
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
                Job::AnalyzeSmartFrame {
                    card,
                    capture,
                    revision,
                    data,
                    cancellation,
                } => self.analyze_smart_frame(card, &capture, revision, data, cancellation),
                Job::ConfigureAfterCapture(policy) => self.after_capture = policy,
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
                let _ = self.outcomes.send(Outcome::Ready(built));
            }
            Err(error) => {
                tracing::warn!(%card, "capture failed: {error}");
                let _ = self.outcomes.send(Outcome::Failed { card, error });
            }
        }
    }

    fn take(&mut self, kind: CaptureKind, card: CardId) -> CliResult<ReadyCapture> {
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
        let mut document = Document::new(capture);
        let frame = Self::prepare_after_capture_revision(&mut document, &self.after_capture)?;
        let bytes = FrameEncoder::new().encode(&frame, ImageFormat::Png)?;
        let thumbnail = Thumbnail::from_frame(&frame, THUMBNAIL_MAX_EDGE).ok();
        let capture_id = self.remember(&document);
        let mut written = Vec::new();
        let consumers =
            Self::dispatch_after_capture(&self.after_capture, &frame, |consumer, frame| {
                match consumer {
                    AfterCaptureConsumer::Copy => SystemClipboard::new()
                        .write_image_reporting(frame)
                        .map(|_| "copied to the clipboard".to_owned())
                        .map_err(|error| error.to_string()),
                    AfterCaptureConsumer::Save => crate::output::export_frame_auto(
                        frame,
                        &Destination::Folder(crate::output::default_directory()),
                    )
                    .and_then(|outcome| {
                        outcome.path.ok_or_else(|| {
                            CliError::Core(CoreError::Storage(
                                "the folder exporter returned no path".to_owned(),
                            ))
                        })
                    })
                    .map(|path| {
                        let displayed = path.display().to_string();
                        written.push(displayed.clone());
                        format!("saved to {displayed}")
                    })
                    .map_err(|error| error.to_string()),
                    AfterCaptureConsumer::Upload => {
                        Err("no upload provider is configured".to_owned())
                    }
                    AfterCaptureConsumer::Overlay => {
                        Ok("queued the shared revision for Quick Access".to_owned())
                    }
                    AfterCaptureConsumer::OpenEditor => {
                        Ok("queued the shared revision for the editor".to_owned())
                    }
                    AfterCaptureConsumer::Pin => {
                        Err("pinning is not available in this build".to_owned())
                    }
                }
            });

        let built = Card {
            id: card,
            capture_id: capture_id.clone(),
            kind,
            source_width: frame.width(),
            source_height: frame.height(),
            scale: frame.scale.get(),
            thumbnail,
            written,
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

        Ok(ReadyCapture {
            card: Box::new(built),
            show_overlay: self.after_capture.overlay,
            open_editor: self.after_capture.open_editor,
            consumers,
        })
    }

    /// Persists a capture, or explains in the log why it was not.
    fn remember(&mut self, document: &Document) -> Option<CaptureId> {
        let store = self.store.as_mut()?;
        match store.insert(NewCapture::new(document)) {
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
        let result = self.current_frame(card, "copy").and_then(|frame| {
            SystemClipboard::new().write_image_reporting(&frame)?;
            Ok("copied to the clipboard".to_owned())
        });
        self.answer(card, result);
    }

    fn save(&mut self, card: CardId) {
        let result = self.current_frame(card, "save").and_then(|frame| {
            let outcome = crate::output::export_frame_auto(
                &frame,
                &Destination::Folder(crate::output::default_directory()),
            )?;
            let path = outcome.path.ok_or_else(|| {
                CliError::Core(CoreError::Storage(
                    "the folder exporter returned no path".to_owned(),
                ))
            })?;
            Ok(format!("saved to {}", path.display()))
        });
        self.answer(card, result);
    }

    fn current_frame(&mut self, card: CardId, verb: &str) -> CliResult<scrozz_core::Frame> {
        let capture_id = self.cached(card, verb)?.capture_id.clone();
        if let (Some(store), Some(capture_id)) = (self.store.as_mut(), capture_id) {
            let state = store.document(&capture_id)?.ok_or_else(|| {
                CliError::Core(CoreError::Storage(format!(
                    "capture {} is no longer in history",
                    capture_id.0
                )))
            })?;
            if let Some(document) = state.complete() {
                return Ok(SkiaRenderer.render(&document)?);
            }
        }
        let bytes = self.cached(card, verb)?.bytes.clone();
        Ok(scrozz_export::decode(&bytes)?)
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

    fn prepare_after_capture_revision(
        document: &mut Document,
        policy: &AfterCapturePolicy,
    ) -> CliResult<scrozz_core::Frame> {
        if policy.apply_smart_frame {
            let current = SkiaRenderer.render(document)?;
            let analysis = analyze_smart_frame(
                &current,
                document.source.provenance,
                &AnalysisCancellation::default(),
            )?;
            document.set_beautification(Some(analysis.beautification))?;
        }
        Ok(SkiaRenderer.render(document)?)
    }

    fn dispatch_after_capture(
        policy: &AfterCapturePolicy,
        frame: &scrozz_core::Frame,
        mut deliver: impl FnMut(
            AfterCaptureConsumer,
            &scrozz_core::Frame,
        ) -> std::result::Result<String, String>,
    ) -> Vec<ConsumerResult> {
        AfterCaptureConsumer::ORDERED
            .into_iter()
            .filter(|consumer| match consumer {
                AfterCaptureConsumer::Copy => policy.copy,
                AfterCaptureConsumer::Save => policy.save,
                AfterCaptureConsumer::Upload => policy.upload,
                AfterCaptureConsumer::Overlay => policy.overlay,
                AfterCaptureConsumer::OpenEditor => policy.open_editor,
                AfterCaptureConsumer::Pin => policy.pin,
            })
            .map(|consumer| ConsumerResult {
                consumer: consumer.slug(),
                result: deliver(consumer, frame),
            })
            .collect()
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

    fn analyze_smart_frame(
        &mut self,
        card: CardId,
        capture: &CaptureId,
        revision: u64,
        data: DocumentData,
        cancellation: AnalysisCancellation,
    ) {
        let fingerprint = match Self::document_fingerprint(&data) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                let _ = self.outcomes.send(Outcome::SmartFrameAnalyzed {
                    card,
                    revision,
                    result: Box::new(Err(error.to_string())),
                });
                return;
            }
        };
        let key = AnalysisCacheKey {
            capture: capture.0.clone(),
            document_fingerprint: fingerprint,
            algorithm_version: scrozz_annotate::smart_frame::SMART_FRAME_ALGORITHM_VERSION,
        };
        if let Some(cached) = self
            .analysis_cache
            .lock()
            .expect("analysis cache mutex poisoned")
            .get(&key)
            .cloned()
        {
            let _ = self.outcomes.send(Outcome::SmartFrameAnalyzed {
                card,
                revision,
                result: Box::new(Ok(cached)),
            });
            return;
        }

        let document = self.document_for_export(capture, data);
        let outcomes = self.outcomes.clone();
        let cache = Arc::clone(&self.analysis_cache);
        let spawn = std::thread::Builder::new()
            .name(format!("scrozz-smart-frame-{}", card.0))
            .spawn(move || {
                let result = document
                    .and_then(|document| {
                        let provenance = document.source.provenance;
                        let frame = SkiaRenderer.render(&document)?;
                        Ok(analyze_smart_frame(&frame, provenance, &cancellation)?)
                    })
                    .map_err(|error| error.to_string());
                if let Ok(analysis) = &result
                    && !cancellation.is_cancelled()
                {
                    let mut cache = cache.lock().expect("analysis cache mutex poisoned");
                    if cache.len() >= MAX_ANALYSIS_CACHE_ENTRIES {
                        cache.clear();
                    }
                    cache.insert(key, analysis.clone());
                }
                let _ = outcomes.send(Outcome::SmartFrameAnalyzed {
                    card,
                    revision,
                    result: Box::new(result),
                });
            });
        if let Err(error) = spawn {
            let _ = self.outcomes.send(Outcome::SmartFrameAnalyzed {
                card,
                revision,
                result: Box::new(Err(format!(
                    "could not start Smart Frame analysis: {error}"
                ))),
            });
        }
    }

    fn cached(&self, card: CardId, verb: &str) -> CliResult<&Cached> {
        self.cache
            .get(&card)
            .ok_or_else(|| CliError::usage(format!("{card} has no capture to {verb}")))
    }

    fn document_fingerprint(data: &DocumentData) -> CliResult<u64> {
        let bytes = serde_json::to_vec(data).map_err(|error| {
            CliError::Core(CoreError::Storage(format!(
                "cannot fingerprint the Smart Frame revision: {error}"
            )))
        })?;
        Ok(bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
        }))
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
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use scrozz_annotate::{
        Annotation, Background, Beautification, BeautificationPreset, RedactStyle, Style,
    };
    use scrozz_core::{
        Capture, ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize, PhysicalSize,
        Provenance, ScaleFactor,
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
                after_capture: AfterCapturePolicy::default(),
                analysis_cache: Arc::new(Mutex::new(HashMap::new())),
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
            "the preserved source colour space should carry an ICC profile chunk"
        );
        assert_eq!(
            scrozz_export::decode(&bytes)
                .expect("decode exported image")
                .color_space,
            ColorSpace::DisplayP3
        );
        fs::remove_dir_all(root).expect("remove export folder");
    }

    #[test]
    fn after_capture_smart_frame_derives_one_revision_without_touching_source() {
        let mut document = Document::new(capture());
        let source = document.source.frame.data.clone();
        let policy = AfterCapturePolicy {
            apply_smart_frame: true,
            ..AfterCapturePolicy::default()
        };

        let derived = Worker::prepare_after_capture_revision(&mut document, &policy).unwrap();

        assert!(document.beautification().is_some());
        assert!(document.beautification().unwrap().auto_balance);
        assert_eq!(document.source.frame.data, source);
        assert!(derived.width() > document.source.frame.width());
    }

    #[test]
    fn after_capture_consumers_are_ordered_isolated_and_share_one_frame() {
        let frame = capture().frame;
        let policy = AfterCapturePolicy {
            apply_smart_frame: true,
            copy: true,
            save: true,
            upload: true,
            overlay: true,
            open_editor: true,
            pin: true,
        };
        let mut seen = Vec::new();
        let expected_address = std::ptr::from_ref(&frame);

        let results = Worker::dispatch_after_capture(&policy, &frame, |consumer, delivered| {
            assert_eq!(std::ptr::from_ref(delivered), expected_address);
            seen.push(consumer);
            if consumer == AfterCaptureConsumer::Upload {
                Err("provider rejected upload".to_owned())
            } else {
                Ok(format!("{} ok", consumer.slug()))
            }
        });

        assert_eq!(seen, AfterCaptureConsumer::ORDERED);
        assert_eq!(results.len(), AfterCaptureConsumer::ORDERED.len());
        assert!(results[2].result.is_err());
        assert!(
            results[3..].iter().all(|result| result.result.is_ok()),
            "an upload failure must not block later consumers"
        );
    }

    #[test]
    fn disabled_after_capture_policy_is_a_byte_stable_noop() {
        let mut document = Document::new(capture());
        let source = document.source.frame.data.clone();
        let output =
            Worker::prepare_after_capture_revision(&mut document, &AfterCapturePolicy::default())
                .unwrap();

        assert!(document.beautification().is_none());
        assert_eq!(document.source.frame.data, source);
        assert_eq!(output.data, source);
    }

    #[test]
    fn card_delivery_uses_the_same_persisted_redacted_revision_as_editor_export() {
        let (mut worker, outcomes, capture_id, root) = stored_worker("revision-parity");
        let mut edited = Document::new(capture());
        edited.add(
            Annotation::Redact {
                area: LogicalRect::new(LogicalPoint::new(4.0, 4.0), LogicalSize::new(20.0, 16.0)),
                style: RedactStyle::Solid,
            },
            Style::redaction(),
        );
        edited
            .set_beautification(Some(Beautification::preset(BeautificationPreset::Social)))
            .unwrap();
        let data = edited.data();
        worker.persist(CardId(7), 1, &capture_id, &data);
        let _ = outcomes.recv().unwrap();

        let delivered = worker.current_frame(CardId(7), "copy").unwrap();
        let expected = SkiaRenderer.render(&edited).unwrap();
        assert_eq!(delivered.data, expected.data);
        assert_eq!(delivered.color_space, expected.color_space);
        fs::remove_dir_all(root).expect("remove scratch store");
    }

    #[test]
    fn analysis_cache_key_tracks_the_document_revision() {
        let first = Document::new(capture()).data();
        let mut second = first.clone();
        second.next_id = second.next_id.saturating_add(1);
        assert_ne!(
            Worker::document_fingerprint(&first).unwrap(),
            Worker::document_fingerprint(&second).unwrap()
        );
    }

    #[test]
    fn asynchronous_analysis_returns_only_the_requested_revision_and_reuses_cache() {
        let (mut worker, outcomes, capture_id, root) = stored_worker("analysis");
        let data = Document::new(capture()).data();
        worker.analyze_smart_frame(
            CardId(7),
            &capture_id,
            41,
            data.clone(),
            AnalysisCancellation::default(),
        );
        let first = outcomes
            .recv_timeout(Duration::from_secs(2))
            .expect("analysis result");
        let Outcome::SmartFrameAnalyzed {
            card,
            revision,
            result,
        } = first
        else {
            panic!("Smart Frame analysis outcome");
        };
        assert_eq!(card, CardId(7));
        assert_eq!(revision, 41);
        assert!(result.is_ok());

        worker.analyze_smart_frame(
            CardId(7),
            &capture_id,
            42,
            data,
            AnalysisCancellation::default(),
        );
        let cached = outcomes
            .recv_timeout(Duration::from_secs(1))
            .expect("cached analysis result");
        assert!(matches!(
            cached,
            Outcome::SmartFrameAnalyzed {
                revision: 42,
                result,
                ..
            } if result.is_ok()
        ));
        fs::remove_dir_all(root).expect("remove scratch store");
    }

    #[test]
    fn cancelled_async_analysis_returns_cancellation_without_caching() {
        let (mut worker, outcomes, capture_id, root) = stored_worker("cancel-analysis");
        let cancellation = AnalysisCancellation::default();
        cancellation.cancel();
        worker.analyze_smart_frame(
            CardId(7),
            &capture_id,
            99,
            Document::new(capture()).data(),
            cancellation,
        );
        let outcome = outcomes
            .recv_timeout(Duration::from_secs(2))
            .expect("cancelled analysis result");
        assert!(matches!(
            outcome,
            Outcome::SmartFrameAnalyzed {
                revision: 99,
                result,
                ..
            } if matches!(result.as_ref(), Err(error) if error.contains("cancelled"))
        ));
        assert!(
            worker
                .analysis_cache
                .lock()
                .expect("analysis cache")
                .is_empty()
        );
        fs::remove_dir_all(root).expect("remove scratch store");
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
