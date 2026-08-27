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
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender, SyncSender, channel, sync_channel},
    },
    thread::JoinHandle,
    time::{Duration, SystemTime},
};

use scrozz_annotate::{Document, Renderer, SkiaRenderer};
use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, CursorMode, Display,
    Error as CoreError, LogicalPoint, LogicalRect, Provenance, ScaleFactor, Window, WindowId,
    WindowSelection,
};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, NamingContext, SystemClipboard};
use scrozz_shell::{ByteSource, DragPayload, DragPreview, byte_source};
use scrozz_stitch::{AtomicCancellation, CancelAction, Progress};
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
    output::CaptureOutput,
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
    /// Re-enumerate and capture the window focused in the in-process picker.
    CommitWindow {
        /// Picker session.
        card: CardId,
        /// Window focused when the user committed.
        window: WindowId,
    },
    /// Close an in-process picker and release its backend.
    CancelWindow(CardId),
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
    /// A scrolling capture reached a meaningful frame boundary.
    Progress {
        /// Which in-flight card the update belongs to.
        card: CardId,
        /// Session status suitable for the HUD and diagnostics.
        progress: Progress,
    },
    /// A capture succeeded and is ready to show.
    Ready(Box<Card>),
    /// The worker prepared an in-process picker snapshot.
    PickWindow {
        /// Picker session.
        card: CardId,
        /// Front-most-first selectable windows.
        windows: Vec<Window>,
        /// Display layout used for highlighting and mixed-DPI scale resolution.
        displays: Vec<Display>,
        /// Why this is a refreshed snapshot, if the previous target vanished.
        notice: Option<String>,
    },
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
    cancellations: Arc<Mutex<HashSet<CardId>>>,
    stopped: Receiver<()>,
    worker: Option<JoinHandle<()>>,
    history_worker: Option<JoinHandle<()>>,
    next_card: u64,
    cancellation: AtomicCancellation,
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
        let (stopped_tx, stopped) = channel();
        let cancellations = Arc::new(Mutex::new(HashSet::new()));
        let worker_cancellations = Arc::clone(&cancellations);
        let cancellation = AtomicCancellation::default();
        let worker_cancellation = cancellation.clone();
        let history_outcomes = outcome_tx.clone();

        let worker = std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || {
                Worker::new(
                    outcome_tx,
                    worker_cancellations,
                    worker_cancellation,
                    retention_policy,
                )
                .run(&job_rx);
                let _ = stopped_tx.send(());
            })
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
            cancellations,
            stopped,
            worker: Some(worker),
            history_worker: Some(history_worker),
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
        if matches!(&job, Job::Capture { .. }) {
            self.cancellation.reset();
        }
        self.jobs.send(job).is_ok()
    }

    /// Cancels a window-selection card immediately, even if its portal call is
    /// currently blocking the worker.
    pub fn cancel_window(&self, card: CardId) -> bool {
        let Ok(mut cancellations) = self.cancellations.lock() else {
            return false;
        };
        cancellations.insert(card);
        drop(cancellations);
        self.jobs.send(Job::CancelWindow(card)).is_ok()
    }

    /// Whether a window-selection card was cancelled by the UI.
    #[must_use]
    pub fn is_window_cancelled(&self, card: CardId) -> bool {
        self.cancellations
            .lock()
            .map_or(true, |cancellations| cancellations.contains(&card))
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
        self.cancellation.cancel(CancelAction::Abort);
        let _ = self.jobs.send(Job::Stop);
        let _ = self.history_queries.send(HistoryQuery::Stop);
        if let Some(worker) = self.worker.take() {
            match self.stopped.recv_timeout(Duration::from_millis(250)) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = worker.join();
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // A Wayland portal chooser is owned by another process and
                    // can remain open indefinitely. Detaching this worker keeps
                    // application shutdown bounded; process exit tears down its
                    // D-Bus request.
                    tracing::debug!("capture worker still blocked during shutdown; detaching");
                }
            }
        }
        if let Some(worker) = self.history_worker.take() {
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
    output: CaptureOutput,
    naming: NamingContext,
    stem: String,
    capture_id: Option<CaptureId>,
}

struct Worker {
    outcomes: Sender<Outcome>,
    store: Option<SqliteStore>,
    cache: HashMap<CardId, Cached>,
    window_pickers: HashMap<CardId, Box<dyn CaptureBackend>>,
    cancellations: Arc<Mutex<HashSet<CardId>>>,
    cancellation: AtomicCancellation,
    retention_policy: RetentionPolicy,
    history_ids: HashMap<CardId, CaptureId>,
    saved: HashMap<CardId, PathBuf>,
}

impl Worker {
    fn new(
        outcomes: Sender<Outcome>,
        cancellations: Arc<Mutex<HashSet<CardId>>>,
        cancellation: AtomicCancellation,
        retention_policy: RetentionPolicy,
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
            window_pickers: HashMap::new(),
            cancellations,
            cancellation,
            retention_policy,
            history_ids: HashMap::new(),
            saved: HashMap::new(),
        }
    }

    fn run(mut self, jobs: &Receiver<Job>) {
        if let Err(error) = self.enforce_current_retention() {
            tracing::warn!("initial source-image retention could not run: {error}");
        }
        while let Ok(job) = jobs.recv() {
            match job {
                Job::Capture { kind, card } => self.capture(kind, card),
                Job::CommitWindow { card, window } => self.commit_window(card, window),
                Job::CancelWindow(card) => {
                    self.window_pickers.remove(&card);
                    self.discard_cancelled(card);
                }
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
        if kind == CaptureKind::Window {
            self.begin_window_capture(card);
            return;
        }

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
            CaptureKind::Fullscreen | CaptureKind::Scrolling => {
                CaptureTarget::Display(backend.active_display()?.id)
            }
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
        let output = CaptureOutput::load()?;

        let request = CaptureRequest {
            target,
            cursor: if output.include_cursor() {
                CursorMode::Visible
            } else {
                CursorMode::Hidden
            },
            include_window_shadow: output.include_window_shadow(),
        };

        let capture = if kind == CaptureKind::Scrolling {
            let outcomes = self.outcomes.clone();
            crate::commands::scrolling_capture_with(
                backend.as_ref(),
                request,
                &mut self.cancellation,
                move |progress| {
                    let _ = outcomes.send(Outcome::Progress { card, progress });
                },
            )?
        } else {
            backend.capture(&request)?
        };
        self.build_card(kind, card, capture, output)
    }

    fn begin_window_capture(&mut self, card: CardId) {
        if self.is_cancelled(card) {
            return;
        }
        let result = (|| -> CliResult<()> {
            let backend = platform::capture_backend()?;
            match backend.window_selection() {
                WindowSelection::InProcess => {
                    let displays = backend.displays()?;
                    let windows = selectable_windows(backend.as_ref())?;
                    if self.is_cancelled(card) {
                        return Ok(());
                    }
                    self.window_pickers.insert(card, backend);
                    let _ = self.outcomes.send(Outcome::PickWindow {
                        card,
                        windows,
                        displays,
                        notice: None,
                    });
                    Ok(())
                }
                WindowSelection::PortalPicker { .. } => {
                    let output = CaptureOutput::load()?;
                    let request = CaptureRequest {
                        target: CaptureTarget::Window(WindowId(
                            "xdg-desktop-portal-picker".to_owned(),
                        )),
                        cursor: if output.include_cursor() {
                            CursorMode::Visible
                        } else {
                            CursorMode::Hidden
                        },
                        include_window_shadow: output.include_window_shadow(),
                    };
                    let capture = backend.capture(&request)?;
                    if self.is_cancelled(card) {
                        return Ok(());
                    }
                    let built = self.build_card(CaptureKind::Window, card, capture, output)?;
                    if self.is_cancelled(card) {
                        return Ok(());
                    }
                    let _ = self.outcomes.send(Outcome::Ready(Box::new(built)));
                    Ok(())
                }
                WindowSelection::Unavailable { why } => {
                    Err(CliError::Core(CoreError::Unsupported {
                        what: "choosing a window".to_owned(),
                        why,
                    }))
                }
            }
        })();

        if let Err(error) = result
            && !self.is_cancelled(card)
        {
            tracing::warn!(%card, "window capture failed: {error}");
            let _ = self.outcomes.send(Outcome::Failed { card, error });
        }
    }

    fn commit_window(&mut self, card: CardId, window: WindowId) {
        if self.is_cancelled(card) {
            self.window_pickers.remove(&card);
            return;
        }
        let result = (|| -> CliResult<(Capture, CaptureOutput)> {
            let backend = self.window_pickers.get(&card).ok_or_else(|| {
                CliError::Core(CoreError::InvalidRequest(format!(
                    "{card} has no active window picker"
                )))
            })?;
            let live = selectable_windows(backend.as_ref())?;
            if !live.iter().any(|candidate| candidate.id == window) {
                return Err(CliError::Core(CoreError::TargetGone(window.0.clone())));
            }
            let output = CaptureOutput::load()?;
            let capture = backend
                .capture(&CaptureRequest {
                    target: CaptureTarget::Window(window.clone()),
                    cursor: if output.include_cursor() {
                        CursorMode::Visible
                    } else {
                        CursorMode::Hidden
                    },
                    include_window_shadow: output.include_window_shadow(),
                })
                .map_err(CliError::Core)?;
            Ok((capture, output))
        })();

        if self.is_cancelled(card) {
            self.window_pickers.remove(&card);
            return;
        }

        match result {
            Ok((capture, output)) => {
                self.window_pickers.remove(&card);
                match self.build_card(CaptureKind::Window, card, capture, output) {
                    Ok(built) => {
                        if !self.is_cancelled(card) {
                            let _ = self.outcomes.send(Outcome::Ready(Box::new(built)));
                        }
                    }
                    Err(error) => {
                        if !self.is_cancelled(card) {
                            let _ = self.outcomes.send(Outcome::Failed { card, error });
                        }
                    }
                }
            }
            Err(CliError::Core(CoreError::TargetGone(_))) => {
                if !self.is_cancelled(card) {
                    self.refresh_window_picker(
                        card,
                        format!("That window closed. Choose another window. ({})", window.0),
                    );
                }
            }
            Err(error) => {
                self.window_pickers.remove(&card);
                if !self.is_cancelled(card) {
                    let _ = self.outcomes.send(Outcome::Failed { card, error });
                }
            }
        }
    }

    fn refresh_window_picker(&mut self, card: CardId, notice: String) {
        if self.is_cancelled(card) {
            return;
        }
        let result = (|| -> CliResult<(Vec<Window>, Vec<Display>)> {
            let backend = self.window_pickers.get(&card).ok_or_else(|| {
                CliError::Core(CoreError::InvalidRequest(format!(
                    "{card} has no active window picker"
                )))
            })?;
            Ok((selectable_windows(backend.as_ref())?, backend.displays()?))
        })();

        match result {
            Ok((windows, displays)) => {
                let _ = self.outcomes.send(Outcome::PickWindow {
                    card,
                    windows,
                    displays,
                    notice: Some(notice),
                });
            }
            Err(error) => {
                self.window_pickers.remove(&card);
                let _ = self.outcomes.send(Outcome::Failed { card, error });
            }
        }
    }

    fn build_card(
        &mut self,
        kind: CaptureKind,
        card: CardId,
        capture: Capture,
        output: CaptureOutput,
    ) -> CliResult<Card> {
        if self.is_cancelled(card) {
            return Err(CliError::Core(CoreError::Cancelled));
        }
        let bytes = output
            .encoder(None)
            .encode(&capture.frame, output.format())?;
        let thumbnail = Thumbnail::from_frame(&capture.frame, THUMBNAIL_MAX_EDGE).ok();
        let capture_id = self.remember(&capture);
        if self.is_cancelled(card) {
            self.discard_capture(capture_id.as_ref(), card);
            return Err(CliError::Core(CoreError::Cancelled));
        }
        if output.copy_to_clipboard()
            && let Err(error) = SystemClipboard::new().write_image_reporting(&capture.frame)
        {
            tracing::warn!("capture succeeded but automatic clipboard copy failed: {error}");
        }

        let naming = NamingContext {
            width: capture.frame.width(),
            height: capture.frame.height(),
            ..NamingContext::now()
        };
        let stem = capture
            .source_app
            .window_title
            .as_deref()
            .or(capture.source_app.name.as_deref())
            .unwrap_or("Scrozz capture")
            .to_owned();
        let built = Card {
            id: card,
            capture_id,
            kind,
            source_width: capture.frame.width(),
            source_height: capture.frame.height(),
            scale: capture.frame.scale.get(),
            source_app: capture.source_app,
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
                output,
                naming,
                stem,
                capture_id: built.capture_id.clone(),
            },
        );
        if let Some(capture_id) = built.capture_id.clone() {
            self.history_ids.insert(card, capture_id);
        }
        self.saved.remove(&card);

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

    fn discard_cancelled(&mut self, card: CardId) {
        let Some(cached) = self.cache.remove(&card) else {
            return;
        };
        let Some(capture_id) = cached.capture_id else {
            return;
        };
        self.discard_capture(Some(&capture_id), card);
    }

    fn discard_capture(&mut self, capture_id: Option<&CaptureId>, card: CardId) {
        let Some(capture_id) = capture_id else {
            return;
        };
        if let Some(store) = self.store.as_mut()
            && let Err(error) = store.delete(capture_id)
        {
            tracing::warn!(%card, "could not discard cancelled capture from history: {error}");
        }
    }

    fn is_cancelled(&self, card: CardId) -> bool {
        self.cancellations
            .lock()
            .map_or(true, |cancellations| cancellations.contains(&card))
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
        let succeeded = result.is_ok();
        self.answer(card, result, true);
        if succeeded {
            self.release(card);
        }
    }

    fn save(&mut self, card: CardId) {
        let export = self.cached(card, "save").map(|cached| {
            let output = cached.output.clone();
            let naming = cached.naming.clone();
            move |bytes: &[u8]| output.export(bytes, &naming)
        });
        let result = export.and_then(|export| self.save_with(card, export));
        let succeeded = result.is_ok();
        self.answer(card, result, true);
        if succeeded {
            self.release(card);
        }
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
        let result = self.render_stored(&capture).and_then(|rendered| {
            let kind = capture_kind(rendered.record.provenance);
            let thumbnail = Thumbnail::from_frame(&rendered.frame, THUMBNAIL_MAX_EDGE).ok();
            let output = CaptureOutput::load()?;
            let naming = NamingContext {
                width: rendered.frame.width(),
                height: rendered.frame.height(),
                ..NamingContext::now()
            };
            let built = Card {
                id: card,
                capture_id: Some(capture.clone()),
                kind,
                source_width: rendered.frame.width(),
                source_height: rendered.frame.height(),
                scale: rendered.frame.scale.get(),
                source_app: rendered.record.source_app.clone(),
                thumbnail,
                written: Vec::new(),
                taken_at: rendered.record.created_at.to_system_time(),
            };
            self.cache.insert(
                card,
                Cached {
                    bytes: Arc::clone(&rendered.bytes),
                    output,
                    naming,
                    stem: stem_for(&rendered.record),
                    capture_id: Some(capture.clone()),
                },
            );
            self.history_ids.insert(card, capture.clone());
            self.saved.remove(&card);
            Ok(built)
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
            Ok(cached) => cached.capture_id.clone().ok_or_else(|| {
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
            let path = CaptureOutput::load()?.export(
                rendered.bytes.as_slice(),
                &NamingContext {
                    width: rendered.frame.width(),
                    height: rendered.frame.height(),
                    ..NamingContext::now()
                },
            )?;
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
        let rendered = self.render_stored(&capture)?;
        let bytes = rendered.bytes.as_ref().clone();
        self.cache.insert(
            card,
            Cached {
                bytes: Arc::new(bytes.clone()),
                output: CaptureOutput::load()?,
                naming: NamingContext {
                    width: rendered.frame.width(),
                    height: rendered.frame.height(),
                    ..NamingContext::now()
                },
                stem: stem_for(&rendered.record),
                capture_id: Some(capture),
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

fn selectable_windows(backend: &dyn CaptureBackend) -> CliResult<Vec<Window>> {
    let mut windows = backend.windows()?;
    windows.retain(|window| !scrozz_ui::picker::is_scrozz_window(window));
    Ok(windows)
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
        app_name: record.source_app.name,
        window_title: record.source_app.window_title,
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
        .source_app
        .window_title
        .as_deref()
        .or(record.source_app.name.as_deref())
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
    use std::cell::Cell;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    };

    use scrozz_store::test_support::{
        ScratchDir, richly_annotated_document, sample_document, sample_frame, scratch_dir,
    };

    use super::*;
    use scrozz_core::{
        ColorSpace, Frame, PhysicalSize, PixelFormat, Provenance, ScaleFactor, SourceApp,
    };

    fn test_output() -> CaptureOutput {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "scrozz-pipeline-output-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut store =
            crate::settings_store::SettingsStore::open(directory.join("settings.json")).unwrap();
        store.set("capture.copy-to-clipboard", "false").unwrap();
        let output = CaptureOutput::from_store(&store).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        output
    }

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
                window_pickers: HashMap::new(),
                cancellations: Arc::new(Mutex::new(HashSet::new())),
                cancellation: AtomicCancellation::default(),
                retention_policy: RetentionPolicy::default(),
                history_ids: HashMap::new(),
                saved: HashMap::new(),
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

    fn worker(store: Option<SqliteStore>) -> (Worker, Receiver<Outcome>) {
        let (outcomes, replies) = channel();
        (
            Worker {
                outcomes,
                store,
                cache: HashMap::new(),
                window_pickers: HashMap::new(),
                cancellations: Arc::new(Mutex::new(HashSet::new())),
                cancellation: AtomicCancellation::default(),
                retention_policy: RetentionPolicy::default(),
                history_ids: HashMap::new(),
                saved: HashMap::new(),
            },
            replies,
        )
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
    fn cancelling_a_window_card_is_visible_before_the_worker_receives_it() {
        let mut pipeline = Pipeline::start().expect("the worker should start");
        let card = pipeline.allocate();

        assert!(pipeline.cancel_window(card));
        assert!(pipeline.is_window_cancelled(card));
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
    fn window_output_keeps_native_edges_stride_and_alpha_without_compositing() {
        let (outcomes, _outcome_rx) = std::sync::mpsc::channel();
        let mut worker = Worker {
            outcomes,
            store: None,
            cache: HashMap::new(),
            window_pickers: HashMap::new(),
            cancellations: Arc::new(Mutex::new(HashSet::new())),
            cancellation: AtomicCancellation::default(),
            retention_policy: RetentionPolicy::default(),
            history_ids: HashMap::new(),
            saved: HashMap::new(),
        };
        let stride = 21;
        let mut data = vec![0xAB; stride * 2];
        data[0..8].copy_from_slice(&[0, 0, 0, 0, 0, 0, 128, 128]);
        data[stride..stride + 8].copy_from_slice(&[128, 0, 0, 128, 0, 255, 0, 255]);
        let source_app = SourceApp {
            name: Some("Browser".into()),
            identifier: Some("com.example.browser".into()),
            window_title: Some("Document".into()),
        };
        let capture = Capture::new(
            Frame {
                data,
                size: PhysicalSize::new(2.0, 2.0),
                stride,
                format: PixelFormat::BgraPremultiplied8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::new(2.0),
            },
            Provenance::Window,
            CaptureTarget::Window(WindowId("42".into())),
        )
        .with_source_app(source_app.clone())
        .with_window_shadow(false);

        let card = worker
            .build_card(CaptureKind::Window, CardId(9), capture, test_output())
            .expect("builds a card");
        let encoded = &worker.cache.get(&CardId(9)).expect("cached").bytes;
        let decoded = scrozz_export::decode(encoded).expect("encoded PNG decodes");

        assert_eq!(card.source_px(), (2, 2), "window bounds must not grow");
        assert_eq!(card.source_app, source_app);
        assert_eq!(decoded.stride, 8, "driver padding must not leak");
        assert_eq!(
            decoded.data,
            vec![0, 0, 0, 0, 255, 0, 0, 128, 0, 0, 255, 128, 0, 255, 0, 255],
            "native transparent and partially transparent edges must survive unchanged"
        );
    }

    #[test]
    fn a_cancelled_window_card_is_not_encoded_or_cached() {
        let (outcomes, _outcome_rx) = std::sync::mpsc::channel();
        let cancellations = Arc::new(Mutex::new(HashSet::from([CardId(9)])));
        let mut worker = Worker {
            outcomes,
            store: None,
            cache: HashMap::new(),
            window_pickers: HashMap::new(),
            cancellations,
            cancellation: AtomicCancellation::default(),
            retention_policy: RetentionPolicy::default(),
            history_ids: HashMap::new(),
            saved: HashMap::new(),
        };
        let capture = Capture::new(
            Frame {
                data: vec![0, 0, 0, 0],
                size: PhysicalSize::new(1.0, 1.0),
                stride: 4,
                format: PixelFormat::BgraPremultiplied8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::new(1.0),
            },
            Provenance::Window,
            CaptureTarget::Window(WindowId("42".into())),
        );

        assert!(matches!(
            worker.build_card(CaptureKind::Window, CardId(9), capture, test_output()),
            Err(CliError::Core(CoreError::Cancelled))
        ));
        assert!(!worker.cache.contains_key(&CardId(9)));
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
                output: test_output(),
                naming: NamingContext::now(),
                stem: "Drag me".to_owned(),
                capture_id: None,
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
                output: test_output(),
                naming: NamingContext::now(),
                stem: "Broken".to_owned(),
                capture_id: None,
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
            window_pickers: HashMap::new(),
            cancellations: Arc::new(Mutex::new(HashSet::new())),
            cancellation: AtomicCancellation::default(),
            retention_policy: RetentionPolicy::default(),
            history_ids: HashMap::new(),
            saved: HashMap::new(),
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
                output: test_output(),
                naming: NamingContext::now(),
                stem: "Leased".to_owned(),
                capture_id: None,
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
                output: test_output(),
                naming: NamingContext::now(),
                stem: "Saved once".to_owned(),
                capture_id: None,
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
