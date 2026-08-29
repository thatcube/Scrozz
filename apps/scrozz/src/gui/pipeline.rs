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
//!
//! # Why the bytes are shared and not private
//!
//! Copy and Save are asynchronous: the user presses a button, the worker does
//! the work, an [`Outcome`] arrives a frame or two later, and nobody minds.
//! A **drag** is not. AppKit will only start a dragging session while the mouse
//! button is still held, so at the instant the gesture commits the main thread
//! must already have the full-resolution PNG in hand — a round trip through
//! this channel would arrive after the button came up, which is the difference
//! between a drop and a card that animates away into nothing.
//!
//! So the encoded bytes live in a [`CaptureVault`]: the worker fills it, the
//! main thread reads it, and both refer to the same `Arc<Vec<u8>>` rather than
//! copying a few hundred kilobytes around. It is the only shared mutable state
//! between the two threads, it is behind one mutex, and no lock is ever held
//! across a capture, an encode or a filesystem call.

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
use scrozz_core::{
    Capture, CaptureRequest, CaptureTarget, ColorSpace, CursorMode, Error as CoreError,
    LogicalPoint, LogicalRect, PinState, Provenance, ScaleFactor, SelectionMode, SelectionOptions,
};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, RgbaImage};
use scrozz_shell::{DragPayload, DragPreview, byte_source};
use scrozz_store::{
    CaptureId, CaptureRecord, DocumentState, FrameHeader, History, NewCapture, Page,
    RetentionPolicy, SearchQuery, SqliteStore, Store,
};
use scrozz_ui::editor::RevisionedFrame;
use scrozz_ui::history::{HistoryEntry, HistoryPage, HistoryThumbnail};

use crate::{
    after_capture::{
        ActionEffect, ActionExecutor, AfterCaptureAction, AfterCaptureSettings, ExecutionReport,
        FinalizedScreenshot, MediaKind, orchestrate,
    },
    fault::{CliError, CliResult},
    gui::{
        action::{CaptureKind, CaptureOrigin},
        card::{
            Card, CardId, PIN_TEXTURE_MAX_EDGE, PinGeneration, PinnedCapture, SurfaceWaker,
            THUMBNAIL_MAX_EDGE, Thumbnail,
        },
        selection::CaptureSelector,
    },
    platform,
};

const MAX_ANALYSIS_CACHE_ENTRIES: usize = 32;

/// Geometry the native drag backend needs from the source window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DragGeometry {
    /// Card rectangle in source-window points or history preview in screen points.
    pub rect: LogicalRect,
    /// Pointer position in the same coordinate space as `rect`.
    pub pointer: LogicalPoint,
}

/// Exact editor state captured when Pin to Screen was requested.
#[derive(Debug)]
pub struct PinEditorSnapshot {
    /// Editor lifetime that produced the snapshot.
    pub generation: u64,
    /// Exact content revision rendered into `rendered`.
    pub revision: u64,
    /// Flattened pixels safe to display immediately.
    pub rendered: RevisionedFrame,
    /// Editable scene persisted before the screen pin is committed.
    pub document: DocumentData,
}

/// Pre-rendered history media ready to become a native drag payload.
#[derive(Clone, Debug)]
pub struct PreparedHistoryDrag {
    stem: String,
    bytes: Arc<Vec<u8>>,
}

impl PreparedHistoryDrag {
    /// Adds gesture geometry without reloading or re-rendering the document.
    pub fn payload(&self, geometry: DragGeometry) -> CliResult<DragPayload> {
        drag_payload(&self.stem, Arc::clone(&self.bytes), geometry)
    }
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
    /// Persist an edited scene graph.
    Edit,
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
            Self::Edit => "save edited document",
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
        /// Where the request entered the app.
        origin: CaptureOrigin,
        /// The identity the resulting card will carry, allocated up front so the
        /// main thread can correlate the answer with the request.
        card: CardId,
        /// The persisted GUI policy snapshotted when this capture was requested.
        policy: AfterCaptureSettings,
    },
    /// Process pixels from the exact one-shot filter returned by Apple's limited picker.
    #[cfg(target_os = "macos")]
    ApplePickerCapture {
        /// The action whose fallback produced the filter.
        kind: CaptureKind,
        /// The original user-action source.
        origin: CaptureOrigin,
        /// The card identity allocated only after Apple returned a selection.
        card: CardId,
        /// Pixels captured from the least-privilege filter.
        capture: scrozz_capture::PickerCapture,
        /// The persisted GUI policy snapshotted when this capture was requested.
        policy: AfterCaptureSettings,
    },
    /// Decode a card's capture so the annotation editor can open on it.
    ///
    /// The decode is the whole reason this is a job: a 6K PNG takes tens of
    /// milliseconds to inflate, which is a visible stutter if it happens
    /// between the click and the window.
    Open(CardId),
    /// Render and encode an editor document ahead of a possible drag.
    PrepareImage {
        /// Which card owns the document.
        card: CardId,
        /// Which opening of that card's editor owns the document.
        generation: u64,
        /// The exact content revision represented by `data`.
        revision: u64,
        /// The editable scene. The worker reconstructs the immutable source
        /// from its own cache.
        data: Box<DocumentData>,
    },
    /// Persist the editor scene graph before its cached source is released.
    PersistDocument {
        /// Card whose durable history identity owns the document.
        card: CardId,
        /// Editor lifetime that produced the snapshot.
        generation: u64,
        /// Exact content revision being persisted.
        revision: u64,
        /// Editable scene graph; source pixels remain in the existing record.
        data: Box<DocumentData>,
    },
    /// Analyse one immutable editor revision without blocking capture work.
    AnalyzeSmartFrame {
        /// Which card owns the editor.
        card: CardId,
        /// Which opening of the editor requested the analysis.
        generation: u64,
        /// Revision echoed back for stale-result rejection.
        revision: u64,
        /// Current annotations with any existing framing removed.
        data: Box<DocumentData>,
        /// Cooperative cancellation shared with the analysis thread.
        cancellation: AnalysisCancellation,
    },
    /// Accept pixels already captured by an interactive region/window selector.
    Captured {
        /// User-facing capture kind that produced the output.
        kind: CaptureKind,
        /// Where the original request entered the app.
        origin: CaptureOrigin,
        /// Identity allocated by the live capture stack.
        card: CardId,
        /// Authoritative pixels and provenance returned by the selector backend.
        capture: Capture,
        /// Persisted GUI policy snapshotted with the request.
        policy: AfterCaptureSettings,
    },
    /// Put a card's capture on the clipboard.
    Copy(CardId),
    /// Put an already-rendered image on the clipboard.
    ///
    /// Used by the editor, which has flattened its own annotations and must not
    /// have the card's unannotated capture substituted for them.
    CopyImage {
        /// Which card the image came from, for the log line.
        card: CardId,
        /// The flattened image and the exact document revision it represents.
        rendered: Box<RevisionedFrame>,
    },
    /// Write an already-rendered image to the configured folder.
    SaveImage {
        /// Which card the image came from, for the log line.
        card: CardId,
        /// The flattened image and the exact document revision it represents.
        rendered: Box<RevisionedFrame>,
    },
    /// Write a card's capture to the configured folder.
    Save(CardId),
    /// Restore a stored capture into a new live card.
    Restore {
        /// Durable capture.
        capture: CaptureId,
        /// Session-local card allocated before posting.
        card: CardId,
    },
    /// Load a stored document for annotation.
    OpenHistoryEditor {
        /// Durable capture to edit.
        capture: CaptureId,
        /// Session-local identity used by the editor output pipeline.
        card: CardId,
    },
    /// Resolve a stored recording's durable media so the video editor can open.
    ///
    /// Separate from [`Job::OpenHistoryEditor`] because the two open different
    /// editors over different documents: a recording has no raster the
    /// annotation editor could accept, and reading its media metadata is a
    /// store read that must not happen on the UI thread.
    OpenHistoryRecording {
        /// Durable capture whose video is wanted.
        capture: CaptureId,
    },
    /// Copy a stored document.
    CopyHistory(CaptureId),
    /// Save a stored document.
    SaveHistory(CaptureId),
    /// Pre-render a stored document before a drag gesture starts.
    PrepareHistoryDrag(CaptureId),
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
    /// Persist a new pin using the capture associated with a live card.
    PinCard {
        /// Live card identity.
        card: CardId,
        /// Durable capture identity supplied by the overlay request.
        capture: CaptureId,
        /// Process-local identity generation for this pin attempt.
        generation: PinGeneration,
        /// Initial durable pin state.
        state: PinState,
        /// Current edited pixels and scene graph, when this card owns an editor.
        editor: Option<Box<PinEditorSnapshot>>,
    },
    /// Persist a changed pin or clear a closed one.
    SetPin {
        /// Durable history identity.
        capture: CaptureId,
        /// Generation whose state is being settled.
        generation: PinGeneration,
        /// `None` means the pin closed.
        state: Option<PinState>,
    },
    /// Clear a pin after every previously queued worker write, then acknowledge it.
    TerminalUnpin {
        /// Durable history identity.
        capture: CaptureId,
        /// Generation of the terminal close request.
        generation: PinGeneration,
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

#[derive(Debug, Clone, PartialEq)]
struct PendingPinUpdate {
    generation: PinGeneration,
    state: Option<PinState>,
}

#[derive(Default)]
struct PendingPinUpdates {
    states: Mutex<HashMap<CaptureId, PendingPinUpdate>>,
}

impl PendingPinUpdates {
    /// Keeps only the newest state per capture and reports whether the worker
    /// needs a wake-up.
    fn queue(
        &self,
        capture: CaptureId,
        generation: PinGeneration,
        state: Option<PinState>,
    ) -> bool {
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let needs_wakeup = states.is_empty();
        if states
            .get(&capture)
            .is_some_and(|pending| pending.generation > generation)
        {
            return false;
        }
        states.insert(capture, PendingPinUpdate { generation, state });
        needs_wakeup
    }

    fn take(&self) -> HashMap<CaptureId, PendingPinUpdate> {
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *states)
    }

    fn cancel_through(&self, capture: &CaptureId, generation: PinGeneration) {
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if states
            .get(capture)
            .is_some_and(|pending| pending.generation <= generation)
        {
            states.remove(capture);
        }
    }
}

/// What the capture thread produced.
#[derive(Debug)]
pub enum Outcome {
    /// A capture succeeded and is ready to show.
    Ready(Box<ReadyCapture>),
    /// A stored capture was rebuilt as a live card.
    Restored(Box<Card>),
    /// A stored recording's durable media was resolved for the video editor.
    HistoryRecording {
        /// Durable capture identity.
        capture: CaptureId,
        /// Absolute path to the media file, checked to exist.
        path: std::path::PathBuf,
        /// Active duration in seconds, as history recorded it.
        duration_secs: f64,
        /// Original source target retained for derivative export provenance.
        target: CaptureTarget,
    },
    /// A card's capture was decoded and the editor can open on it.
    Opened {
        /// Which card.
        card: CardId,
        /// The complete editable document.
        document: Box<Document>,
        /// Whether no overlay card owns this editor artifact.
        editor_only: bool,
    },
    /// An editor revision is encoded and ready for synchronous drag use.
    Prepared {
        /// Which card owns the rendered revision.
        card: CardId,
        /// Which opening of the editor produced it.
        generation: u64,
        /// The revision now available in the vault.
        revision: u64,
    },
    /// An editor revision could not be prepared.
    PreparationFailed {
        /// Which card owns the document.
        card: CardId,
        /// Which opening of the editor produced it.
        generation: u64,
        /// The revision that failed.
        revision: u64,
        /// Why it failed.
        error: CliError,
    },
    /// Smart Frame analysis completed or failed.
    SmartFrameAnalyzed {
        /// Which card owns the editor.
        card: CardId,
        /// Which opening of the editor requested the analysis.
        generation: u64,
        /// Revision the result belongs to.
        revision: u64,
        /// Fully resolved framing or an actionable failure.
        result: Box<std::result::Result<SmartFrameAnalysis, String>>,
    },
    /// A capture failed. The main thread says why and shows nothing.
    Failed {
        /// Which card was expected.
        card: CardId,
        /// Which action may be retried after a permission change.
        kind: CaptureKind,
        /// The original user-action source.
        origin: CaptureOrigin,
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
    PinCreated {
        /// Source card retired only after the UI commits this settlement.
        card: CardId,
        /// Durable history identity.
        capture: CaptureId,
        /// Generation of the provisional pin this result belongs to.
        generation: PinGeneration,
        /// Pixels only; geometry and presentation remain UI-owned.
        texture: Option<Thumbnail>,
        /// Non-fatal high-resolution texture failure.
        warning: Option<CliError>,
    },
    /// The first durable pin write failed, so the visible viewport must roll back.
    PinCreationFailed {
        /// Durable pin identity to remove from the overlay.
        capture: CaptureId,
        /// Generation of the failed provisional pin.
        generation: PinGeneration,
        /// Explicit storage or identity error.
        error: CliError,
    },
    /// Persisting a pin update failed.
    PinPersistenceFailed {
        /// Capture whose sidecar could not be updated.
        capture: CaptureId,
        /// Generation whose write failed.
        generation: PinGeneration,
        /// Explicit storage error.
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
    /// A selected history item is pre-rendered for immediate drag startup.
    HistoryDragPrepared {
        /// Durable capture.
        capture: CaptureId,
        /// Rendered bytes and stable file label.
        prepared: PreparedHistoryDrag,
    },
}

/// A finalized card plus every isolated ambient action result.
#[derive(Debug)]
pub struct ReadyCapture {
    /// The surface model built from the immutable artifact.
    pub card: Card,
    /// Enabled actions in deterministic execution order.
    pub actions: ExecutionReport,
}

/// A handle to the capture and history threads.
pub struct Pipeline {
    jobs: Sender<Job>,
    pending_pin_updates: Arc<PendingPinUpdates>,
    history_queries: Sender<HistoryQuery>,
    outcomes: Receiver<Outcome>,
    worker: Option<JoinHandle<()>>,
    history_worker: Option<JoinHandle<()>>,
    next_card: u64,
    vault: CaptureVault,
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
    pub fn start(selector: Arc<dyn CaptureSelector>) -> CliResult<Self> {
        Self::start_with_history(selector, true)
    }

    /// Starts the worker with persistent history explicitly enabled or disabled.
    ///
    /// Sealed/headless tests disable it so validation cannot create or migrate a
    /// developer's real profile.
    pub fn start_with_history(
        selector: Arc<dyn CaptureSelector>,
        history_enabled: bool,
    ) -> CliResult<Self> {
        Self::start_with_history_and_waker(selector, history_enabled, None)
    }

    /// Starts the worker with explicit history behavior and an event-loop wake hook.
    pub fn start_with_history_and_waker(
        selector: Arc<dyn CaptureSelector>,
        history_enabled: bool,
        waker: Option<SurfaceWaker>,
    ) -> CliResult<Self> {
        Self::start_with_history_waker_and_retention(
            selector,
            history_enabled,
            waker,
            RetentionPolicy::default(),
        )
    }

    /// Starts both workers with explicit history, wake, and retention behavior.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Core`] under the same conditions as [`Self::start`].
    pub fn start_with_history_waker_and_retention(
        selector: Arc<dyn CaptureSelector>,
        history_enabled: bool,
        waker: Option<SurfaceWaker>,
        retention_policy: RetentionPolicy,
    ) -> CliResult<Self> {
        let (jobs, job_rx) = channel();
        let (history_queries, history_rx) = channel();
        let (outcome_tx, outcomes) = channel();
        let vault = CaptureVault::new();
        let worker_vault = vault.clone();
        let pending_pin_updates = Arc::new(PendingPinUpdates::default());
        let worker_pin_updates = Arc::clone(&pending_pin_updates);
        let history_outcomes = outcome_tx.clone();
        let history_waker = waker.clone();

        let worker = std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || {
                Worker::new(
                    outcome_tx,
                    selector,
                    worker_vault,
                    history_enabled,
                    waker,
                    retention_policy,
                )
                .run(&job_rx, &worker_pin_updates);
            })
            .map_err(|err| {
                CliError::Core(CoreError::Platform(format!(
                    "could not start the capture worker: {err}"
                )))
            })?;
        let history_worker = match std::thread::Builder::new()
            .name("scrozz-history".to_owned())
            .spawn(move || {
                HistoryReader::new(history_outcomes, history_waker, history_enabled)
                    .run(&history_rx);
            }) {
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
            pending_pin_updates,
            history_queries,
            outcomes,
            worker: Some(worker),
            history_worker: Some(history_worker),
            next_card: 1,
            vault,
        })
    }

    /// The encoded captures, for callers that need the bytes synchronously.
    ///
    /// The one such caller is a drag: it must write a real file while the mouse
    /// button is still down. Everything else should post a [`Job`] instead.
    #[must_use]
    pub fn captures(&self) -> CaptureVault {
        self.vault.clone()
    }

    /// Allocates the next card identity.
    pub const fn allocate(&mut self) -> CardId {
        let id = CardId(self.next_card);
        self.next_card += 1;
        id
    }

    /// Posts a job. Returns `false` if the worker has gone.
    pub fn post(&self, job: Job) -> bool {
        if let Job::SetPin {
            capture,
            generation,
            state,
        } = job
        {
            if state.is_none() {
                // Closing is terminal for all geometry already queued by that
                // viewport. If the worker took an older state concurrently,
                // this direct job follows it in the channel and clears it.
                self.pending_pin_updates
                    .cancel_through(&capture, generation);
                return self
                    .jobs
                    .send(Job::SetPin {
                        capture,
                        generation,
                        state,
                    })
                    .is_ok();
            }
            if !self.pending_pin_updates.queue(capture, generation, state) {
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

    /// Feed a completed selector capture into the ordinary durable card pipeline.
    ///
    /// Region and window selectors own their platform-specific lifecycle, but
    /// their output must still receive history identity, a bounded texture, and
    /// the same Pin to Screen action as a fixed display capture.
    pub fn accept_capture(
        &mut self,
        kind: CaptureKind,
        origin: CaptureOrigin,
        capture: Capture,
        policy: AfterCaptureSettings,
    ) -> CliResult<CardId> {
        let card = self.allocate();
        self.jobs
            .send(Job::Captured {
                kind,
                origin,
                card,
                capture,
                policy,
            })
            .map_err(|_| {
                CliError::Core(CoreError::Platform(
                    "the capture worker stopped before it could accept selector pixels".into(),
                ))
            })?;
        Ok(card)
    }

    /// Clears a pin after all older worker jobs and waits for the durable result.
    pub fn terminal_unpin(&self, capture: CaptureId, generation: PinGeneration) -> CliResult<()> {
        self.pending_pin_updates
            .cancel_through(&capture, generation);
        let (reply, result) = channel();
        self.jobs
            .send(Job::TerminalUnpin {
                capture,
                generation,
                reply,
            })
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
#[derive(Clone)]
struct Cached {
    /// The untouched card capture. This remains the default after the editor
    /// closes, per D14.
    bytes: CaptureBytes,
    /// Original source pixels for rebuilding an editable history document.
    ///
    /// Restored cards display a flattened render in `bytes`, but editor
    /// revisions must start from this source or existing edits would be baked
    /// in and applied a second time.
    editor_source: Option<Arc<Vec<u8>>>,
    /// A revision prepared only for an active editor's output and drag paths.
    rendered: Option<CaptureBytes>,
    /// Where the pixels came from, kept alongside them.
    ///
    /// The cached PNG cannot carry this, and decision D9 turns on it: a window
    /// capture may gain an outer canvas but must refuse subject styling wherever
    /// it is reconstructed.
    provenance: Provenance,
    /// What was aimed at, for the same reason.
    target: CaptureTarget,
    /// The display scale the pixels were captured at.
    ///
    /// A PNG has no notion of backing scale, so `decode` can only return
    /// `IDENTITY`. Restoring the original matters because every logical
    /// coordinate in the editor is derived from it: reopening a Retina capture
    /// at 1.0 would double every annotation's apparent size and export at the
    /// wrong resolution.
    scale: ScaleFactor,
    /// How the samples should be interpreted.
    ///
    /// `decode` reports `Unknown` for the same reason — the encoder does not
    /// round-trip a colour profile — and a capture that was known to be
    /// Display P3 must not silently become unlabelled on its way to the editor.
    color_space: ColorSpace,
    /// Durable history identity used to validate a card-to-pin transition.
    capture_id: Option<CaptureId>,
}

struct ScreenshotExecutor<'a> {
    policy: &'a AfterCaptureSettings,
}

impl ActionExecutor<FinalizedScreenshot<'_>> for ScreenshotExecutor<'_> {
    fn execute(
        &mut self,
        action: AfterCaptureAction,
        artifact: &FinalizedScreenshot<'_>,
    ) -> std::result::Result<ActionEffect, String> {
        match action {
            AfterCaptureAction::CopyToClipboard => {
                scrozz_shell::write_capture_to_clipboard(artifact.frame, artifact.png)
                    .map(|_| ActionEffect::Completed)
                    .map_err(|error| error.to_string())
            }
            AfterCaptureAction::SaveAutomatically => {
                crate::output::export_with_settings(artifact.png, self.policy)
                    .map(ActionEffect::Saved)
                    .map_err(|error| error.to_string())
            }
            AfterCaptureAction::UploadAndCopyLink => Err(
                "no cloud upload provider is implemented or configured in this build".to_owned(),
            ),
            AfterCaptureAction::ShowRecentCapturesOverlay => {
                Ok(ActionEffect::ShowRecentCapturesOverlay)
            }
            AfterCaptureAction::OpenEditor => Ok(ActionEffect::OpenEditor),
            AfterCaptureAction::PinToScreen => {
                Err("Pin to Screen is not implemented in this build".to_owned())
            }
        }
    }
}

/// What is kept for a card that is still on screen.
#[derive(Clone)]
pub struct CaptureBytes {
    /// The editor lifetime these bytes belong to. `None` is the untouched card
    /// capture.
    pub(crate) generation: Option<u64>,
    /// The document revision these bytes represent. An untouched capture is
    /// revision zero.
    pub(crate) revision: u64,
    /// The full-resolution PNG. What a copy, a save or a drag hands over.
    pub full: Arc<Vec<u8>>,
    /// A small PNG of the same capture, for a drag image. `None` if the
    /// thumbnail could not be encoded, which costs a drag its picture but not
    /// its payload.
    pub preview: Option<Arc<Vec<u8>>>,
}

impl CaptureBytes {
    /// Encodes one exact editor render ahead of synchronous drag use.
    pub(crate) fn from_rendered(generation: u64, rendered: &RevisionedFrame) -> CliResult<Self> {
        let full = FrameEncoder::new()
            .encode(rendered.frame(), ImageFormat::Png)
            .map(Arc::new)?;
        let preview = Thumbnail::from_frame(rendered.frame(), THUMBNAIL_MAX_EDGE)
            .ok()
            .as_ref()
            .and_then(preview_png)
            .map(Arc::new);
        Ok(Self {
            generation: Some(generation),
            revision: rendered.revision(),
            full,
            preview,
        })
    }

    /// The document revision encoded in these bytes.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// The editor lifetime these bytes were rendered for.
    #[must_use]
    pub const fn generation(&self) -> Option<u64> {
        self.generation
    }
}

/// The encoded captures of every card still in the pile.
///
/// Cheap to clone: both ends hold the same map. See the module header for why
/// this is shared rather than owned by the worker.
#[derive(Clone, Default)]
pub struct CaptureVault {
    inner: Arc<Mutex<HashMap<CardId, Cached>>>,
}

impl CaptureVault {
    /// An empty vault.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Files a freshly encoded capture.
    fn store(&self, card: CardId, cached: Cached) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(card, cached);
        }
    }

    /// Seeds a vault entry without taking a real screenshot.
    #[cfg(test)]
    pub(crate) fn store_test_capture(&self, card: CardId, capture: &Capture) -> CliResult<()> {
        let full = FrameEncoder::new()
            .encode(&capture.frame, ImageFormat::Png)
            .map(Arc::new)?;
        self.store(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full,
                    preview: None,
                },
                editor_source: None,
                rendered: None,
                provenance: capture.provenance,
                target: capture.target.clone(),
                scale: capture.frame.scale,
                color_space: capture.frame.color_space,
                capture_id: None,
            },
        );
        Ok(())
    }

    /// The bytes for `card`, if it is still in the pile.
    ///
    /// Returns a clone of the handles, not of the bytes, so the lock is held
    /// for a pointer copy and nothing else.
    #[must_use]
    pub fn get(&self, card: CardId) -> Option<CaptureBytes> {
        self.inner
            .lock()
            .ok()?
            .get(&card)
            .map(|cached| cached.bytes.clone())
    }

    /// Gets only bytes that match `revision`.
    ///
    /// The original capture is revision zero. Later revisions come from the
    /// renderer and are kept separately so closing the editor restores the
    /// card's untouched source rather than silently replacing it.
    #[must_use]
    pub fn get_revision(
        &self,
        card: CardId,
        generation: u64,
        revision: u64,
    ) -> Option<CaptureBytes> {
        let map = self.inner.lock().ok()?;
        let cached = map.get(&card)?;
        cached
            .rendered
            .as_ref()
            .filter(|bytes| bytes.generation() == Some(generation) && bytes.revision() == revision)
            .cloned()
    }

    /// Installs a prepared editor revision without replacing the card capture.
    fn store_rendered(&self, card: CardId, bytes: CaptureBytes) -> bool {
        let Ok(mut map) = self.inner.lock() else {
            return false;
        };
        let Some(cached) = map.get_mut(&card) else {
            return false;
        };
        let version = (bytes.generation().unwrap_or(0), bytes.revision());
        let should_replace = cached.rendered.as_ref().is_none_or(|current| {
            (current.generation().unwrap_or(0), current.revision()) <= version
        });
        if should_replace {
            cached.rendered = Some(bytes);
        }
        true
    }

    /// The full cached entry, including metadata needed to reopen the editor.
    fn cached(&self, card: CardId) -> Option<Cached> {
        self.inner.lock().ok()?.get(&card).cloned()
    }

    /// Drops a card's bytes. Called when the card leaves the pile.
    ///
    /// A drag that is still in flight is unaffected: the file it advertised was
    /// already written, and its own bytes are held by the drag session.
    fn forget(&self, card: CardId) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(&card);
        }
    }

    /// How many captures are being held. For tests and diagnostics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map_or(0, |map| map.len())
    }

    /// Whether anything is being held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct Worker {
    outcomes: Sender<Outcome>,
    waker: Option<SurfaceWaker>,
    selector: Arc<dyn CaptureSelector>,
    store: Option<SqliteStore>,
    vault: CaptureVault,
    pin_generations: HashMap<CaptureId, PinGeneration>,
    retention_policy: RetentionPolicy,
    derived_documents: HashMap<CardId, DocumentData>,
    analysis_cache: Arc<Mutex<HashMap<AnalysisCacheKey, SmartFrameAnalysis>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct AnalysisCacheKey {
    card: CardId,
    document_fingerprint: u64,
    algorithm_version: u16,
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
    fn new(
        outcomes: Sender<Outcome>,
        selector: Arc<dyn CaptureSelector>,
        vault: CaptureVault,
        history_enabled: bool,
        waker: Option<SurfaceWaker>,
        retention_policy: RetentionPolicy,
    ) -> Self {
        // Opened once, here, rather than per capture: the schema check and the
        // directory creation are not free, and doing them on the shutter path
        // would put them between the keypress and the card.
        let store = if !history_enabled {
            None
        } else {
            match SqliteStore::open_default() {
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
            }
        };

        let mut worker = Self {
            outcomes,
            waker,
            selector,
            store,
            vault,
            pin_generations: HashMap::new(),
            retention_policy,
            derived_documents: HashMap::new(),
            analysis_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        worker.restore_existing_pins();
        worker
    }

    fn run(mut self, jobs: &Receiver<Job>, pending_pin_updates: &PendingPinUpdates) {
        if self.store.is_some()
            && let Err(error) = self.enforce_current_retention()
        {
            tracing::warn!("initial source-image retention could not run: {error}");
        }
        while let Ok(job) = jobs.recv() {
            match job {
                Job::Capture {
                    kind,
                    card,
                    origin,
                    policy,
                } => self.capture(kind, card, origin, &policy),
                #[cfg(target_os = "macos")]
                Job::ApplePickerCapture {
                    kind,
                    origin,
                    card,
                    capture,
                    policy,
                } => self.capture_apple_picker(kind, origin, card, capture, &policy),
                Job::Open(card) => self.open(card),
                Job::PrepareImage {
                    card,
                    generation,
                    revision,
                    data,
                } => self.prepare_image(card, generation, revision, *data),
                Job::PersistDocument {
                    card,
                    generation,
                    revision,
                    data,
                } => self.persist_document(card, generation, revision, &data),
                Job::AnalyzeSmartFrame {
                    card,
                    generation,
                    revision,
                    data,
                    cancellation,
                } => self.analyze_smart_frame(card, generation, revision, *data, cancellation),
                Job::Captured {
                    kind,
                    origin,
                    card,
                    capture,
                    policy,
                } => self.accept_captured(kind, origin, card, capture, &policy),
                Job::Copy(card) => self.copy(card),
                Job::CopyImage { card, rendered } => self.copy_image(card, &rendered),
                Job::SaveImage { card, rendered } => self.save_image(card, &rendered),
                Job::Save(card) => self.save(card),
                Job::Release(card) => {
                    self.vault.forget(card);
                    self.derived_documents.remove(&card);
                }
                Job::PinCard {
                    card,
                    capture,
                    generation,
                    state,
                    editor,
                } => {
                    if self.claim_pin_generation(&capture, generation) {
                        self.pin_card(card, &capture, generation, &state, editor.as_deref());
                    }
                }
                Job::SetPin {
                    capture,
                    generation,
                    state,
                } => {
                    if self.claim_pin_generation(&capture, generation) {
                        self.set_pin(&capture, generation, state.as_ref());
                    }
                }
                Job::TerminalUnpin {
                    capture,
                    generation,
                    reply,
                } => {
                    let result = if self.claim_pin_generation(&capture, generation) {
                        self.persist_pin(&capture, None)
                    } else {
                        Ok(())
                    };
                    let _ = reply.send(result);
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
                    for (capture, pending) in pending_pin_updates.take() {
                        if self.claim_pin_generation(&capture, pending.generation) {
                            self.set_pin(&capture, pending.generation, pending.state.as_ref());
                        }
                    }
                }
                Job::Restore { capture, card } => self.restore(capture, card),
                Job::OpenHistoryEditor { capture, card } => {
                    self.open_history_editor(capture, card);
                }
                Job::OpenHistoryRecording { capture } => self.open_history_recording(capture),
                Job::CopyHistory(capture) => self.copy_history(capture),
                Job::SaveHistory(capture) => self.save_history(capture),
                Job::PrepareHistoryDrag(capture) => self.prepare_history_drag(capture),
                Job::SetPinned { capture, pinned } => self.set_pinned(capture, pinned),
                Job::Delete(capture) => self.delete(capture),
                Job::EnforceRetention(policy) => self.set_retention_policy(policy),
                Job::Stop => break,
            }
        }
        tracing::debug!("capture worker stopped");
    }

    fn capture(
        &mut self,
        kind: CaptureKind,
        card: CardId,
        origin: CaptureOrigin,
        policy: &AfterCaptureSettings,
    ) {
        tracing::debug!(
            %card,
            capture = kind.label(),
            origin = origin.label(),
            "capture job started"
        );
        let mut lifecycle = CaptureLifecycle::new(Arc::clone(&self.selector));
        let result = self.take(kind, card, policy, &mut lifecycle);
        match result {
            Ok(built) => {
                self.emit(Outcome::Ready(Box::new(built)));
            }
            Err(error) if error.is_cancellation() => {
                tracing::debug!(%card, origin = origin.label(), "capture selection cancelled");
            }
            Err(error) => {
                tracing::warn!(%card, origin = origin.label(), "capture failed: {error}");
                self.emit(Outcome::Failed {
                    card,
                    kind,
                    origin,
                    error,
                });
            }
        }
    }

    fn accept_captured(
        &mut self,
        kind: CaptureKind,
        origin: CaptureOrigin,
        card: CardId,
        capture: Capture,
        policy: &AfterCaptureSettings,
    ) {
        match self.finish_capture(kind, card, capture, policy) {
            Ok(ready) => self.emit(Outcome::Ready(Box::new(ready))),
            Err(error) => {
                tracing::warn!(%card, origin = origin.label(), "selector capture could not enter the card pipeline: {error}");
                self.emit(Outcome::Failed {
                    card,
                    kind,
                    origin,
                    error,
                });
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn capture_apple_picker(
        &mut self,
        kind: CaptureKind,
        origin: CaptureOrigin,
        card: CardId,
        picker_capture: scrozz_capture::PickerCapture,
        policy: &AfterCaptureSettings,
    ) {
        let result = picker_capture
            .into_capture()
            .map_err(CliError::Core)
            .and_then(|capture| self.finish_capture(kind, card, capture, policy));

        match result {
            Ok(card) => {
                self.emit(Outcome::Ready(Box::new(card)));
            }
            Err(error) if error.is_cancellation() => {
                tracing::debug!(%card, "Apple picker capture cancelled");
            }
            Err(error) => {
                tracing::warn!(%card, origin = origin.label(), "Apple picker capture failed: {error}");
                self.emit(Outcome::Failed {
                    card,
                    kind,
                    origin,
                    error,
                });
            }
        }
    }

    fn take(
        &mut self,
        kind: CaptureKind,
        card: CardId,
        policy: &AfterCaptureSettings,
        lifecycle: &mut CaptureLifecycle,
    ) -> CliResult<ReadyCapture> {
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
        self.finish_capture(kind, card, capture, policy)
    }

    fn finish_capture(
        &mut self,
        kind: CaptureKind,
        card: CardId,
        capture: Capture,
        policy: &AfterCaptureSettings,
    ) -> CliResult<ReadyCapture> {
        let mut document = Document::new(capture);
        let apply_smart_frame = crate::settings::smart_frame_after_capture(policy)?;
        let editor_source = if apply_smart_frame {
            Some(Arc::new(
                FrameEncoder::new().encode(&document.source.frame, ImageFormat::Png)?,
            ))
        } else {
            None
        };
        let frame = Self::prepare_after_capture_revision(&mut document, apply_smart_frame)?;
        let bytes = Arc::new(FrameEncoder::new().encode(&frame, ImageFormat::Png)?);

        // Finalized bytes and metadata exist before any action runs. Copy is
        // first and happens before thumbnailing or history I/O; durable
        // destinations follow, then host presentation effects are returned.
        let artifact = FinalizedScreenshot {
            frame: &frame,
            png: bytes.as_slice(),
        };
        let mut executor = ScreenshotExecutor { policy };
        let actions = orchestrate(MediaKind::Screenshot, policy, &artifact, &mut executor);

        let thumbnail = Thumbnail::from_frame(&frame, THUMBNAIL_MAX_EDGE).ok();

        // Encoded here, on the worker, because the only caller is a drag and a
        // drag has no time to spare once it has started.
        let preview = thumbnail.as_ref().and_then(preview_png).map(Arc::new);
        let capture_id = self.remember_document(&document);
        if apply_smart_frame {
            self.derived_documents.insert(card, document.data());
        }
        self.vault.store(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::clone(&bytes),
                    preview,
                },
                editor_source,
                rendered: None,
                provenance: document.source.provenance,
                target: document.source.target.clone(),
                scale: frame.scale,
                color_space: frame.color_space,
                capture_id: capture_id.clone(),
            },
        );

        let written = actions
            .steps
            .iter()
            .filter_map(|step| match &step.outcome {
                crate::after_capture::ActionOutcome::Succeeded(ActionEffect::Saved(path)) => {
                    Some(path.display().to_string())
                }
                crate::after_capture::ActionOutcome::Succeeded(ActionEffect::Uploaded(url)) => {
                    Some(url.clone())
                }
                _ => None,
            })
            .collect();

        Ok(ReadyCapture {
            card: Card {
                id: card,
                media: scrozz_ui::card::CardMedia::Image,
                capture_id,
                kind,
                provenance: document.source.provenance,
                source_width: frame.width(),
                source_height: frame.height(),
                scale: frame.scale.get(),
                thumbnail,
                // History persistence is internal and does not count as an export.
                // A visible file is created only when the user presses Save; writing
                // one here made Save create a duplicate a few seconds later.
                written,
                taken_at: SystemTime::now(),
            },
            actions,
        })
    }

    fn prepare_after_capture_revision(
        document: &mut Document,
        apply_smart_frame: bool,
    ) -> CliResult<scrozz_core::Frame> {
        if !apply_smart_frame {
            return Ok(document.source.frame.clone());
        }
        let current = SkiaRenderer.render(document)?;
        let analysis = analyze_smart_frame(
            &current,
            document.source.provenance,
            &AnalysisCancellation::default(),
        )?;
        document.set_beautification(Some(analysis.beautification))?;
        Ok(SkiaRenderer.render(document)?)
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

    fn prepare_image(&mut self, card: CardId, generation: u64, revision: u64, data: DocumentData) {
        let result = self
            .capture_from_cache(card, "prepare for drag")
            .and_then(|capture| Document::from_data(capture, data).map_err(CliError::Core))
            .and_then(|document| {
                RevisionedFrame::from_document(&document, revision).map_err(CliError::Core)
            })
            .and_then(|rendered| CaptureBytes::from_rendered(generation, &rendered));
        let outcome = match result {
            Ok(bytes) => {
                if self.vault.store_rendered(card, bytes) {
                    Outcome::Prepared {
                        card,
                        generation,
                        revision,
                    }
                } else {
                    Outcome::PreparationFailed {
                        card,
                        generation,
                        revision,
                        error: CliError::usage(format!(
                            "{card} disappeared before revision {revision} was prepared"
                        )),
                    }
                }
            }
            Err(error) => Outcome::PreparationFailed {
                card,
                generation,
                revision,
                error,
            },
        };
        self.emit(outcome);
    }

    fn persist_document(
        &mut self,
        card: CardId,
        generation: u64,
        revision: u64,
        data: &DocumentData,
    ) {
        let result = self
            .cached_entry(card, "save its edits")
            .and_then(|cached| {
                cached.capture_id.ok_or_else(|| {
                    CliError::Core(CoreError::Storage(format!(
                        "{card} was captured while history was unavailable"
                    )))
                })
            })
            .and_then(|capture| {
                self.history_store()?.save_edits(&capture, data)?;
                Ok(capture)
            });
        match result {
            Ok(capture) => self.history_done(
                HistoryOperation::Edit,
                Some(capture),
                None,
                format!("{card} editor {generation} revision {revision} saved to capture history"),
            ),
            Err(error) => self.emit(Outcome::Refused { card, error }),
        }
    }

    /// Persists a capture, or explains in the log why it was not.
    #[cfg(test)]
    fn remember(&mut self, capture: &Capture) -> Option<CaptureId> {
        self.remember_document(&Document::new(capture.clone()))
    }

    fn remember_document(&mut self, document: &Document) -> Option<CaptureId> {
        let policy = self.retention_policy.clone();
        let store = self.store.as_mut()?;
        match store.insert(NewCapture::of_kind(
            document,
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

    /// Decodes a card's capture and hands it to the editor.
    ///
    /// The provenance travels with it because decision D9 allows only an outer
    /// presentation canvas for window captures; the editor must know which kind
    /// it holds rather than guess.
    fn open(&mut self, card: CardId) {
        let durable = self.vault.cached(card).and_then(|cached| cached.capture_id);
        let document = match durable {
            Some(capture) => match self.load_document(&capture) {
                Ok(document) => Ok(document),
                Err(error) => {
                    tracing::warn!(
                        %card,
                        %error,
                        "stored document could not be loaded; opening its flattened visible pixels"
                    );
                    self.open_cached_document(card)
                }
            },
            None => self.open_cached_document(card),
        };
        match document {
            Ok(document) => {
                self.emit(Outcome::Opened {
                    card,
                    document: Box::new(document),
                    editor_only: false,
                });
            }
            Err(error) => {
                self.emit(Outcome::Refused { card, error });
            }
        }
    }

    fn open_cached_document(&self, card: CardId) -> CliResult<Document> {
        match self.derived_documents.get(&card).cloned() {
            Some(data) => self
                .capture_from_cache(card, "open")
                .and_then(|capture| Document::from_data(capture, data).map_err(CliError::Core)),
            None => self
                .capture_from_visible_cache(card, "open")
                .map(Document::new),
        }
    }

    fn analyze_smart_frame(
        &mut self,
        card: CardId,
        generation: u64,
        revision: u64,
        data: DocumentData,
        cancellation: AnalysisCancellation,
    ) {
        let document_fingerprint = match Self::document_fingerprint(&data) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                self.emit(Outcome::SmartFrameAnalyzed {
                    card,
                    generation,
                    revision,
                    result: Box::new(Err(error.to_string())),
                });
                return;
            }
        };
        let result = self
            .capture_from_cache(card, "analyse")
            .and_then(|capture| Document::from_data(capture, data).map_err(CliError::Core));
        let document = match result {
            Ok(document) => document,
            Err(error) => {
                self.emit(Outcome::SmartFrameAnalyzed {
                    card,
                    generation,
                    revision,
                    result: Box::new(Err(error.to_string())),
                });
                return;
            }
        };
        let key = AnalysisCacheKey {
            card,
            document_fingerprint,
            algorithm_version: scrozz_annotate::smart_frame::SMART_FRAME_ALGORITHM_VERSION,
        };
        if cancellation.is_cancelled() {
            self.emit(Outcome::SmartFrameAnalyzed {
                card,
                generation,
                revision,
                result: Box::new(Err("Smart Frame analysis was cancelled".to_owned())),
            });
            return;
        }
        if let Some(cached) = self
            .analysis_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned()
        {
            self.emit(Outcome::SmartFrameAnalyzed {
                card,
                generation,
                revision,
                result: Box::new(Ok(cached)),
            });
            return;
        }

        let outcomes = self.outcomes.clone();
        let waker = self.waker.clone();
        let cache = Arc::clone(&self.analysis_cache);
        let spawn = std::thread::Builder::new()
            .name(format!("scrozz-smart-frame-{}", card.0))
            .spawn(move || {
                let provenance = document.source.provenance;
                let result = SkiaRenderer
                    .render(&document)
                    .and_then(|frame| analyze_smart_frame(&frame, provenance, &cancellation))
                    .map_err(|error| error.to_string());
                if let Ok(analysis) = &result
                    && !cancellation.is_cancelled()
                {
                    let mut cache = cache
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if cache.len() >= MAX_ANALYSIS_CACHE_ENTRIES {
                        cache.clear();
                    }
                    cache.insert(key, analysis.clone());
                }
                if outcomes
                    .send(Outcome::SmartFrameAnalyzed {
                        card,
                        generation,
                        revision,
                        result: Box::new(result),
                    })
                    .is_ok()
                    && let Some(waker) = waker
                {
                    waker();
                }
            });
        if let Err(error) = spawn {
            self.emit(Outcome::SmartFrameAnalyzed {
                card,
                generation,
                revision,
                result: Box::new(Err(format!(
                    "could not start Smart Frame analysis: {error}"
                ))),
            });
        }
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

    fn copy_image(&mut self, card: CardId, rendered: &RevisionedFrame) {
        tracing::debug!(
            %card,
            revision = rendered.revision(),
            "copying a rendered document revision"
        );
        let result = FrameEncoder::new()
            .encode(rendered.frame(), ImageFormat::Png)
            .and_then(|png| {
                scrozz_shell::write_capture_to_clipboard(rendered.frame(), &png).map(|_| ())
            })
            .map(|()| "copied the annotated image".to_owned())
            .map_err(CliError::from);
        self.answer(card, result);
    }

    fn save_image(&mut self, card: CardId, rendered: &RevisionedFrame) {
        tracing::debug!(
            %card,
            revision = rendered.revision(),
            "saving a rendered document revision"
        );
        let result = FrameEncoder::new()
            .encode(rendered.frame(), ImageFormat::Png)
            .map_err(CliError::from)
            .and_then(|bytes| {
                let path = crate::output::export_default(&bytes)?;
                Ok(format!("saved the annotated image to {}", path.display()))
            });
        self.answer(card, result);
    }

    fn copy(&mut self, card: CardId) {
        // The round trip through PNG is deliberate — see the module docs — and
        // is also what will make "copy" work for a card whose capture arrived
        // over IPC, where the worker never held a `Frame` at all.
        let result = self.cached_entry(card, "copy").and_then(|cached| {
            let mut frame = scrozz_export::decode(&cached.bytes.full)?;
            frame.scale = cached.scale;
            frame.color_space = cached.color_space;
            scrozz_shell::write_capture_to_clipboard(&frame, &cached.bytes.full)?;
            Ok("copied to the clipboard".to_owned())
        });
        self.answer(card, result);
    }

    fn save(&mut self, card: CardId) {
        let result = self.cached(card, "save").and_then(|cached| {
            let path = crate::output::export_default(&cached.full)?;
            Ok(format!("saved to {}", path.display()))
        });
        self.answer(card, result);
    }

    fn cached(&self, card: CardId, verb: &str) -> CliResult<CaptureBytes> {
        self.vault
            .get(card)
            .ok_or_else(|| CliError::usage(format!("{card} has no capture to {verb}")))
    }

    fn restore(&mut self, capture: CaptureId, card: CardId) {
        let result = self.render_stored(&capture).map(|rendered| {
            let kind = capture_kind(rendered.record.provenance);
            let thumbnail = Thumbnail::from_frame(&rendered.frame, THUMBNAIL_MAX_EDGE).ok();
            let preview = thumbnail.as_ref().and_then(preview_png).map(Arc::new);
            let built = Card {
                id: card,
                media: scrozz_ui::card::CardMedia::Image,
                capture_id: Some(capture.clone()),
                kind,
                provenance: rendered.record.provenance,
                source_width: rendered.frame.width(),
                source_height: rendered.frame.height(),
                scale: rendered.frame.scale.get(),
                thumbnail,
                written: Vec::new(),
                taken_at: rendered.record.created_at.to_system_time(),
            };
            self.vault.store(
                card,
                Cached {
                    bytes: CaptureBytes {
                        generation: None,
                        revision: 0,
                        full: Arc::clone(&rendered.bytes),
                        preview,
                    },
                    editor_source: Some(Arc::clone(&rendered.source_bytes)),
                    rendered: None,
                    provenance: rendered.record.provenance,
                    target: rendered.record.target.clone(),
                    scale: rendered.frame.scale,
                    color_space: rendered.frame.color_space,
                    capture_id: Some(capture.clone()),
                },
            );
            built
        });

        match result {
            Ok(card) => {
                self.emit(Outcome::Restored(Box::new(card)));
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

    /// Reads one stored recording's durable media path and hands it back.
    ///
    /// The file is checked here rather than assumed: a history row survives its
    /// media being moved or deleted by the user — history never owned those
    /// bytes — and the honest answer in that case is a named failure, not an
    /// editor over a file that is not there.
    fn open_history_recording(&mut self, capture: CaptureId) {
        let record = self
            .history_store()
            .and_then(|store| store.record(&capture).map_err(CliError::Core))
            .and_then(|record| record.ok_or_else(|| history_not_found(&capture)));
        let resolved = record.and_then(|record| {
            let video = record.video.ok_or_else(|| {
                CliError::Core(CoreError::InvalidRequest(format!(
                    "capture {:?} is not a recording",
                    capture.0
                )))
            })?;
            let path = video.path;
            if !path.is_file() {
                return Err(CliError::Core(CoreError::Storage(format!(
                    "the recording at {} is no longer on disk",
                    path.display()
                ))));
            }
            Ok((path, video.duration_secs, record.target))
        });
        match resolved {
            Ok((path, duration_secs, target)) => {
                self.emit(Outcome::HistoryRecording {
                    capture: capture.clone(),
                    path,
                    duration_secs,
                    target,
                });
                self.history_done(
                    HistoryOperation::OpenEditor,
                    Some(capture),
                    None,
                    "opened in the video editor".to_owned(),
                );
            }
            Err(error) => self.history_failed(HistoryOperation::OpenEditor, Some(capture), error),
        }
    }

    fn open_history_editor(&mut self, capture: CaptureId, card: CardId) {
        let result = self.render_stored(&capture).map(|rendered| {
            let thumbnail = Thumbnail::from_frame(&rendered.frame, THUMBNAIL_MAX_EDGE).ok();
            let preview = thumbnail.as_ref().and_then(preview_png).map(Arc::new);
            self.vault.store(
                card,
                Cached {
                    bytes: CaptureBytes {
                        generation: None,
                        revision: 0,
                        full: Arc::clone(&rendered.bytes),
                        preview,
                    },
                    editor_source: Some(Arc::clone(&rendered.source_bytes)),
                    rendered: None,
                    provenance: rendered.record.provenance,
                    target: rendered.record.target.clone(),
                    scale: rendered.frame.scale,
                    color_space: rendered.frame.color_space,
                    capture_id: Some(capture.clone()),
                },
            );
            rendered.document
        });
        match result {
            Ok(document) => {
                self.emit(Outcome::Opened {
                    card,
                    document: Box::new(document),
                    editor_only: true,
                });
                self.history_done(
                    HistoryOperation::OpenEditor,
                    Some(capture),
                    None,
                    "opened in the annotation editor".to_owned(),
                );
            }
            Err(error) => {
                self.history_failed(HistoryOperation::OpenEditor, Some(capture), error);
            }
        }
    }

    fn copy_history(&mut self, capture: CaptureId) {
        let result = self.render_stored(&capture).and_then(|rendered| {
            scrozz_shell::write_capture_to_clipboard(&rendered.frame, &rendered.bytes)?;
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

    fn prepare_history_drag(&mut self, capture: CaptureId) {
        let result = self
            .render_stored(&capture)
            .map(|rendered| PreparedHistoryDrag {
                stem: stem_for(&rendered.record),
                bytes: rendered.bytes,
            });
        match result {
            Ok(prepared) => {
                self.emit(Outcome::HistoryDragPrepared { capture, prepared });
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
            Some(DocumentState::Complete(document)) => Ok(*document),
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
        let source_bytes =
            Arc::new(FrameEncoder::new().encode(&document.source.frame, ImageFormat::Png)?);
        let bytes = Arc::new(FrameEncoder::new().encode(&frame, ImageFormat::Png)?);
        Ok(RenderedStored {
            record,
            document,
            frame,
            bytes,
            source_bytes,
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
        self.emit(Outcome::HistoryDone {
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
        self.emit(Outcome::HistoryFailed {
            request: None,
            operation,
            capture,
            error,
        });
    }

    fn cached_entry(&self, card: CardId, verb: &str) -> CliResult<Cached> {
        self.vault
            .cached(card)
            .ok_or_else(|| CliError::usage(format!("{card} has no capture to {verb}")))
    }

    fn capture_from_cache(&self, card: CardId, verb: &str) -> CliResult<Capture> {
        self.capture_from_cached_bytes(card, verb, true)
    }

    fn capture_from_visible_cache(&self, card: CardId, verb: &str) -> CliResult<Capture> {
        self.capture_from_cached_bytes(card, verb, false)
    }

    fn capture_from_cached_bytes(
        &self,
        card: CardId,
        verb: &str,
        prefer_editor_source: bool,
    ) -> CliResult<Capture> {
        let cached = self.cached_entry(card, verb)?;
        let source = if prefer_editor_source {
            cached.editor_source.as_ref().unwrap_or(&cached.bytes.full)
        } else {
            &cached.bytes.full
        };
        let mut frame = scrozz_export::decode(source)?;
        // PNG carries neither backing scale nor colour space. Restore both from
        // capture metadata before editor geometry or rendering sees it.
        frame.scale = cached.scale;
        frame.color_space = cached.color_space;
        Ok(Capture {
            frame,
            provenance: cached.provenance,
            target: cached.target,
        })
    }

    fn pin_card(
        &mut self,
        card: CardId,
        capture: &CaptureId,
        generation: PinGeneration,
        state: &PinState,
        editor: Option<&PinEditorSnapshot>,
    ) {
        let cached_capture = self.vault.cached(card).and_then(|cached| cached.capture_id);
        if cached_capture.as_ref() != Some(capture) {
            let error = CliError::usage(format!(
                "{card} does not own persisted capture {}",
                capture.0
            ));
            self.emit(Outcome::PinCreationFailed {
                capture: capture.clone(),
                generation,
                error,
            });
            return;
        }
        let result = (|| {
            if let Some(editor) = editor {
                self.history_store()?
                    .save_edits(capture, &editor.document)?;
                tracing::debug!(
                    capture = %capture.0,
                    editor_generation = editor.generation,
                    revision = editor.revision,
                    "persisted edited document before committing its screen pin"
                );
            }
            self.persist_pin(capture, Some(state))
        })();
        match result {
            Ok(()) => {
                let texture = editor
                    .map(|editor| {
                        Thumbnail::from_frame(editor.rendered.frame(), PIN_TEXTURE_MAX_EDGE)
                            .map_err(CliError::from)
                    })
                    .unwrap_or_else(|| self.load_pin_texture(capture));
                let (texture, warning) = match texture {
                    Ok(texture) => (Some(texture), None),
                    Err(error) => (None, Some(error)),
                };
                self.emit(Outcome::PinCreated {
                    card,
                    capture: capture.clone(),
                    generation,
                    texture,
                    warning,
                });
            }
            Err(error) => {
                self.emit(Outcome::PinCreationFailed {
                    capture: capture.clone(),
                    generation,
                    error,
                });
            }
        }
    }

    fn set_pin(
        &mut self,
        capture: &CaptureId,
        generation: PinGeneration,
        state: Option<&PinState>,
    ) {
        if let Err(error) = self.persist_pin(capture, state) {
            self.emit(Outcome::PinPersistenceFailed {
                capture: capture.clone(),
                generation,
                error,
            });
        }
    }

    fn claim_pin_generation(&mut self, capture: &CaptureId, generation: PinGeneration) -> bool {
        if self
            .pin_generations
            .get(capture)
            .is_some_and(|settled| *settled > generation)
        {
            return false;
        }
        self.pin_generations.insert(capture.clone(), generation);
        true
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
                self.emit(Outcome::PinReady(Box::new(pin)));
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
        let (source_width, source_height, scale, geometry_error) =
            record.frame.as_ref().map_or_else(
                || {
                    (
                        420,
                        180,
                        1.0,
                        Some("pinned capture has no still-frame geometry".to_owned()),
                    )
                },
                safe_pin_source_geometry,
            );
        let (mut texture, pixel_error) = match self.load_pin_texture(id) {
            Ok(texture) => (Some(texture), None),
            Err(error) => {
                tracing::warn!(capture = %id.0, "could not load persisted pin pixels: {error}");
                (None, Some(error.to_string()))
            }
        };
        if geometry_error.is_some() {
            texture = None;
        }
        let content_error = [geometry_error, pixel_error]
            .into_iter()
            .flatten()
            .reduce(|left, right| format!("{left}; {right}"));
        Some(PinnedCapture {
            id: id.clone(),
            name: record
                .window_title
                .clone()
                .or(record.app_name.clone())
                .unwrap_or_else(|| format!("Capture {}", id.0)),
            provenance: record.provenance,
            source_width,
            source_height,
            scale,
            state,
            texture,
            content_error,
        })
    }

    fn load_pin_texture(&mut self, id: &CaptureId) -> CliResult<Thumbnail> {
        let store = self.store.as_mut().ok_or_else(|| {
            CliError::Core(CoreError::Storage(
                "history is unavailable, so pin pixels cannot be loaded".into(),
            ))
        })?;
        match store.document(id) {
            Ok(Some(DocumentState::Complete(document))) => {
                let frame = SkiaRenderer::new().render(&document)?;
                Thumbnail::from_frame(&frame, PIN_TEXTURE_MAX_EDGE).map_err(CliError::from)
            }
            Ok(Some(DocumentState::ImageEvicted(_))) | Ok(None) => {
                Err(CliError::Core(CoreError::Storage(format!(
                    "pinned capture {} still has recoverable state, but its source pixels are unavailable",
                    id.0
                ))))
            }
            Err(err) => Err(CliError::from(err)),
        }
    }

    fn answer(&self, card: CardId, result: CliResult<String>) {
        let message = match result {
            Ok(detail) => Outcome::Done { card, detail },
            Err(error) => Outcome::Refused { card, error },
        };
        self.emit(message);
    }

    fn emit(&self, outcome: Outcome) {
        if self.outcomes.send(outcome).is_ok()
            && let Some(waker) = &self.waker
        {
            waker();
        }
    }
}

fn safe_pin_source_geometry(frame: &FrameHeader) -> (u32, u32, f64, Option<String>) {
    let width = checked_source_dimension(frame.size.width);
    let height = checked_source_dimension(frame.size.height);
    let scale = frame.scale.get();
    if let (Some(width), Some(height)) = (width, height)
        && scale.is_finite()
        && scale > 0.0
    {
        return (width, height, scale, None);
    }
    (
        420,
        180,
        1.0,
        Some(
            "Pinned capture metadata contains invalid physical dimensions; a safe error viewport was used"
                .into(),
        ),
    )
}

fn checked_source_dimension(value: f64) -> Option<u32> {
    (value.is_finite() && value >= 1.0 && value <= f64::from(u32::MAX))
        .then(|| value.round() as u32)
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

/// Encodes a card's thumbnail as a PNG, for use as a drag image.
///
/// Small on purpose: this is the picture that follows the cursor, not the
/// payload. A failure is logged and swallowed — a drag without a picture is a
/// cosmetic loss, a drag refused because a picture would not encode is not.
fn preview_png(thumbnail: &Thumbnail) -> Option<Vec<u8>> {
    let image = RgbaImage {
        width: thumbnail.width(),
        height: thumbnail.height(),
        data: thumbnail.pixels().to_vec(),
    };
    match FrameEncoder::new().encode_rgba(&image, ColorSpace::Srgb, ImageFormat::Png) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            tracing::debug!("the drag preview could not be encoded: {err}");
            None
        }
    }
}

struct HistoryReader {
    outcomes: Sender<Outcome>,
    waker: Option<SurfaceWaker>,
    store: Option<SqliteStore>,
    open_default: bool,
}

impl HistoryReader {
    fn new(outcomes: Sender<Outcome>, waker: Option<SurfaceWaker>, history_enabled: bool) -> Self {
        Self {
            outcomes,
            waker,
            store: None,
            open_default: history_enabled,
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
        if self.outcomes.send(outcome).is_ok()
            && let Some(waker) = &self.waker
        {
            waker();
        }
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
    document: Document,
    frame: scrozz_core::Frame,
    bytes: Arc<Vec<u8>>,
    source_bytes: Arc<Vec<u8>>,
}

fn history_entry(
    store: &mut SqliteStore,
    record: CaptureRecord,
) -> CliResult<Option<HistoryEntry>> {
    // A recording has no editable document and never will: history holds a
    // reference to a durable media file, not pixels. Reading one is not a
    // failure and must not drop the row — the whole point of a video history
    // entry is that it is still there to open, reveal and delete after the
    // poster, the size and even the file itself have gone.
    if let Some(video) = &record.video {
        let missing = !video.path.is_file();
        let (width, height, scale) = record_geometry(&record);
        return Ok(Some(HistoryEntry {
            id: record.id,
            created_at: record.created_at,
            media_kind: record.media_kind,
            pinned: record.pinned,
            app_name: record.app_name,
            window_title: record.window_title,
            width,
            height,
            scale,
            image_present: !missing,
            content_error: missing
                .then(|| format!("This recording is no longer at {}.", video.path.display())),
            annotation_count: record.annotation_count,
            ocr_text: record.ocr_text,
            thumbnail: None,
        }));
    }

    let loaded = store.document(&record.id);
    let (image_present, thumbnail, content_error) = match loaded {
        Ok(None) => return Ok(None),
        Ok(Some(DocumentState::ImageEvicted(_))) => (false, None, None),
        Ok(Some(DocumentState::Complete(document))) => {
            let rendered = (|| {
                let preview = history_thumbnail(&document, document.source.frame.scale)?;
                let rgba = scrozz_export::to_straight_rgba8(&preview)?;
                HistoryThumbnail::from_rgba(rgba.width, rgba.height, rgba.data).ok_or_else(|| {
                    CliError::Core(CoreError::Codec(format!(
                        "capture {} produced malformed thumbnail pixels",
                        record.id.0
                    )))
                })
            })();
            match rendered {
                Ok(thumbnail) => (true, Some(thumbnail), None),
                Err(error) => {
                    tracing::warn!(
                        capture = %record.id.0,
                        %error,
                        "history entry is visible but its content cannot be rendered"
                    );
                    (
                        true,
                        None,
                        Some(format!(
                            "This capture cannot be opened by this build: {error}"
                        )),
                    )
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                capture = %record.id.0,
                %error,
                "history entry is visible but its document cannot be decoded"
            );
            (
                matches!(&record.image, scrozz_store::ImageState::Present { .. }),
                None,
                Some(format!(
                    "This capture cannot be opened by this build: {error}"
                )),
            )
        }
    };
    let (width, height, scale) = record_geometry(&record);
    Ok(Some(HistoryEntry {
        id: record.id,
        created_at: record.created_at,
        media_kind: record.media_kind,
        pinned: record.pinned,
        app_name: record.app_name,
        window_title: record.window_title,
        width,
        height,
        scale,
        image_present,
        content_error,
        annotation_count: record.annotation_count,
        ocr_text: record.ocr_text,
        thumbnail,
    }))
}

fn record_geometry(record: &CaptureRecord) -> (u32, u32, f64) {
    if let Some(frame) = &record.frame {
        return (
            dimension(frame.size.width),
            dimension(frame.size.height),
            frame.scale.get(),
        );
    }
    let size = record.video.as_ref().and_then(|video| video.size);
    let width = size.map_or(0, |size| dimension(size.width));
    let height = size.map_or(0, |size| dimension(size.height));
    (width, height, 1.0)
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
    use std::sync::Arc;

    use scrozz_store::test_support::{
        ScratchDir, richly_annotated_document, sample_document, scratch_dir,
    };

    use super::*;
    use scrozz_core::{
        ColorSpace, Error, Frame, PhysicalSize, PixelFormat, Provenance, RegionSelector,
        Result as CoreResult, ScaleFactor, SelectionCapabilities, SelectionOutcome, WindowId,
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
        Pipeline::start_with_history(Arc::new(RefusingSelector), false)
            .expect("the worker should start")
    }

    /// Builds a worker whose cache is seeded by hand.
    ///
    /// The real cache is filled by an actual capture, which a unit test has no
    /// way to perform, so the reconstruction path is exercised directly.
    fn worker_holding(card: CardId, cached: Cached) -> (Worker, Receiver<Outcome>) {
        let (outcomes, inbox) = std::sync::mpsc::channel();
        let vault = CaptureVault::new();
        vault.store(card, cached);
        let worker = Worker {
            outcomes,
            waker: None,
            selector: Arc::new(RefusingSelector),
            store: None,
            vault,
            pin_generations: HashMap::new(),
            retention_policy: RetentionPolicy::default(),
            derived_documents: HashMap::new(),
            analysis_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        (worker, inbox)
    }

    fn one_pixel_png() -> Vec<u8> {
        let frame = Frame {
            data: vec![255, 255, 255, 255],
            size: scrozz_core::PhysicalSize::new(1.0, 1.0),
            stride: 4,
            format: scrozz_core::PixelFormat::Rgba8,
            color_space: scrozz_core::ColorSpace::Srgb,
            scale: scrozz_core::ScaleFactor::new(1.0),
        };
        FrameEncoder::new()
            .encode(&frame, ImageFormat::Png)
            .expect("a one pixel frame should encode")
    }

    fn two_by_two_png() -> Vec<u8> {
        let frame = Frame {
            data: vec![255; 16],
            size: scrozz_core::PhysicalSize::new(2.0, 2.0),
            stride: 8,
            format: scrozz_core::PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::new(2.0),
        };
        FrameEncoder::new()
            .encode(&frame, ImageFormat::Png)
            .expect("a two by two frame should encode")
    }

    #[test]
    fn opening_a_capture_restores_the_scale_the_png_could_not_carry() {
        // A PNG has no backing-scale field, so `decode` can only return
        // identity. If the cache did not remember the real one, reopening a
        // Retina capture would halve its logical size: every annotation would
        // land at twice its intended scale and the export would come out at
        // the wrong resolution. This is why the metadata rides alongside the
        // bytes rather than being re-derived from them.
        let card = CardId(1);
        let (mut worker, inbox) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(one_pixel_png()),
                    preview: None,
                },
                editor_source: None,
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::AllDisplays,
                scale: ScaleFactor::new(2.0),
                color_space: ColorSpace::DisplayP3,
                capture_id: None,
            },
        );
        worker.open(card);

        let Some(Outcome::Opened { document, .. }) = inbox.try_iter().next() else {
            panic!("opening a cached card should produce a capture");
        };
        assert!(
            (document.source.frame.scale.get() - 2.0).abs() < f64::EPSILON,
            "the capture's own scale should survive the cache, not decode's identity"
        );
        assert_eq!(
            document.source.frame.color_space,
            ColorSpace::DisplayP3,
            "a wide-gamut capture must not become unlabelled on the way to the editor"
        );
    }

    #[test]
    fn a_restored_scale_gives_the_editor_the_right_logical_size() {
        // The consequence, stated as the thing that actually matters: logical
        // geometry is physical size divided by scale, and the editor lays every
        // annotation out in logical coordinates.
        let card = CardId(1);
        let (mut worker, inbox) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(two_by_two_png()),
                    preview: None,
                },
                editor_source: None,
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::AllDisplays,
                scale: ScaleFactor::new(2.0),
                color_space: ColorSpace::Srgb,
                capture_id: None,
            },
        );
        worker.open(card);

        let Some(Outcome::Opened { document, .. }) = inbox.try_iter().next() else {
            panic!("opening a cached card should produce a capture");
        };
        let logical = f64::from(document.source.frame.width()) / document.source.frame.scale.get();
        assert!(
            (logical - 1.0).abs() < f64::EPSILON,
            "a 2x2 physical capture at 2.0 scale is 1x1 logical, not 2x2"
        );
    }

    #[test]
    fn an_unknown_colour_space_is_preserved_as_unknown() {
        // The cache restores what it was told, including the absence of a
        // profile. It must not upgrade `Unknown` to `Srgb` on the way through.
        let card = CardId(1);
        let (mut worker, inbox) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(one_pixel_png()),
                    preview: None,
                },
                editor_source: None,
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::AllDisplays,
                scale: ScaleFactor::IDENTITY,
                color_space: ColorSpace::Unknown,
                capture_id: None,
            },
        );
        worker.open(card);

        let Some(Outcome::Opened { document, .. }) = inbox.try_iter().next() else {
            panic!("opening a cached card should produce a capture");
        };
        assert_eq!(document.source.frame.color_space, ColorSpace::Unknown);
    }

    #[test]
    fn opening_a_window_capture_keeps_it_a_window_capture() {
        // D9: a window capture may receive an outer presentation canvas but its
        // subject pixels may not be restyled. The cached PNG carries no
        // provenance, so the metadata alongside it is load-bearing.
        let card = CardId(1);
        let (mut worker, inbox) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(one_pixel_png()),
                    preview: None,
                },
                editor_source: None,
                rendered: None,
                provenance: Provenance::Window,
                target: CaptureTarget::Window(scrozz_core::WindowId("test-window".to_owned())),
                scale: ScaleFactor::IDENTITY,
                color_space: ColorSpace::Srgb,
                capture_id: None,
            },
        );
        worker.open(card);

        let Some(Outcome::Opened { document, .. }) = inbox.try_iter().next() else {
            panic!("opening a cached card should produce a capture");
        };
        assert_eq!(document.source.provenance, Provenance::Window);
        assert!(document.may_beautify());
        assert!(!document.may_style_subject());
    }

    #[test]
    fn opening_a_region_capture_still_allows_beautification() {
        let card = CardId(1);
        let bounds = scrozz_core::LogicalRect::new(
            scrozz_core::LogicalPoint::new(0.0, 0.0),
            scrozz_core::LogicalSize::new(1.0, 1.0),
        );
        let (mut worker, inbox) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(one_pixel_png()),
                    preview: None,
                },
                editor_source: None,
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::Region(bounds),
                scale: ScaleFactor::IDENTITY,
                color_space: ColorSpace::Srgb,
                capture_id: None,
            },
        );
        worker.open(card);

        let Some(Outcome::Opened { document, .. }) = inbox.try_iter().next() else {
            panic!("opening a cached card should produce a capture");
        };
        assert_eq!(document.source.provenance, Provenance::Region);
        assert!(document.may_beautify());
    }

    #[test]
    fn opening_a_card_that_was_never_captured_is_refused() {
        let (mut worker, inbox) = worker_holding(
            CardId(1),
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(one_pixel_png()),
                    preview: None,
                },
                editor_source: None,
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::AllDisplays,
                scale: ScaleFactor::IDENTITY,
                color_space: ColorSpace::Srgb,
                capture_id: None,
            },
        );
        worker.open(CardId(9));
        assert!(matches!(
            inbox.try_iter().next(),
            Some(Outcome::Refused { card, .. }) if card == CardId(9)
        ));
    }

    #[test]
    fn unavailable_stored_edits_fall_back_to_flattened_pixels_not_raw_source() {
        let card = CardId(10);
        let (mut worker, inbox) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(two_by_two_png()),
                    preview: None,
                },
                editor_source: Some(Arc::new(one_pixel_png())),
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::AllDisplays,
                scale: ScaleFactor::IDENTITY,
                color_space: ColorSpace::Srgb,
                capture_id: Some(CaptureId("stored-redaction".into())),
            },
        );

        worker.open(card);

        let Some(Outcome::Opened { document, .. }) = inbox.try_iter().next() else {
            panic!("flattened fallback should remain editable");
        };
        assert_eq!(document.source.frame.width(), 2);
        assert_eq!(document.source.frame.height(), 2);
    }

    fn sample_capture(provenance: Provenance) -> Capture {
        Capture {
            frame: Frame {
                data: vec![255; 4 * 4 * 4],
                size: PhysicalSize::new(4.0, 4.0),
                stride: 16,
                format: PixelFormat::Rgba8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::new(2.0),
            },
            provenance,
            target: CaptureTarget::Window(WindowId("selector-window".into())),
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("scrozz-pin-{name}-{}-{nonce}", std::process::id()))
    }

    fn no_after_capture_actions() -> AfterCaptureSettings {
        let mut settings = AfterCaptureSettings::fresh();
        for action in AfterCaptureAction::EXECUTION_ORDER {
            settings.set(MediaKind::Screenshot, action, false);
        }
        settings
    }

    fn worker_with_store(label: &str) -> (ScratchDir, Worker, Receiver<Outcome>) {
        let dir = scratch_dir(label);
        let store = SqliteStore::open_ephemeral(dir.path()).expect("open isolated history");
        let (outcomes, receiver) = channel();
        (
            dir,
            Worker {
                outcomes,
                waker: None,
                selector: Arc::new(RefusingSelector),
                store: Some(store),
                vault: CaptureVault::new(),
                pin_generations: HashMap::new(),
                retention_policy: RetentionPolicy::default(),
                derived_documents: HashMap::new(),
                analysis_cache: Arc::new(Mutex::new(HashMap::new())),
            },
            receiver,
        )
    }

    fn received(receiver: &Receiver<Outcome>) -> Outcome {
        receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("worker outcome")
    }

    #[test]
    fn after_capture_smart_frame_is_one_derived_revision_before_consumers() {
        let (_dir, mut worker, _outcomes) = worker_with_store("smart-frame-after-capture");
        let mut policy = no_after_capture_actions();
        policy.set_value(crate::settings::APPLY_SMART_FRAME_AFTER_CAPTURE_KEY, "true");
        let source = sample_document(96, 60, 1, 0).source;
        let source_pixels = source.frame.data.clone();

        let ready = worker
            .finish_capture(CaptureKind::Region, CardId(7), source, &policy)
            .unwrap();

        assert!(ready.card.source_width > 96);
        assert!(worker.derived_documents.contains_key(&CardId(7)));
        let capture = ready.card.capture_id.expect("history identity");
        let stored = worker
            .store
            .as_mut()
            .unwrap()
            .document(&capture)
            .unwrap()
            .and_then(scrozz_store::DocumentState::complete)
            .expect("complete derived document");
        assert!(stored.beautification().is_some());
        assert!(stored.beautification().unwrap().auto_balance);
        assert_eq!(stored.source.frame.data, source_pixels);
    }

    #[test]
    fn disabled_after_capture_smart_frame_is_a_byte_stable_noop() {
        let mut document = sample_document(32, 20, 1, 0);
        let source = document.source.frame.clone();

        let output = Worker::prepare_after_capture_revision(&mut document, false).unwrap();

        assert!(document.beautification().is_none());
        assert_eq!(output.data, source.data);
        assert_eq!(output.size, source.size);
        assert_eq!(output.stride, source.stride);
        assert_eq!(output.format, source.format);
        assert_eq!(output.color_space, source.color_space);
        assert_eq!(output.scale, source.scale);
    }

    #[test]
    fn after_capture_smart_frame_reopens_as_editable_without_history() {
        let card = CardId(12);
        let source = sample_document(96, 60, 1, 0).source;
        let seed_png = FrameEncoder::new()
            .encode(&source.frame, ImageFormat::Png)
            .unwrap();
        let (mut worker, outcomes) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(seed_png),
                    preview: None,
                },
                editor_source: None,
                rendered: None,
                provenance: source.provenance,
                target: source.target.clone(),
                scale: source.frame.scale,
                color_space: source.frame.color_space,
                capture_id: None,
            },
        );
        let mut policy = no_after_capture_actions();
        policy.set_value(crate::settings::APPLY_SMART_FRAME_AFTER_CAPTURE_KEY, "true");
        worker
            .finish_capture(CaptureKind::Region, card, source, &policy)
            .unwrap();
        worker.open(card);

        let Outcome::Opened { document, .. } = received(&outcomes) else {
            panic!("derived document should reopen");
        };
        assert!(document.beautification().is_some());
        assert_eq!(document.source.frame.width(), 96);
        assert!(
            document
                .beautification()
                .is_some_and(|beautification| beautification.auto_balance)
        );
    }

    #[test]
    fn asynchronous_smart_frame_analysis_is_revision_bound_and_cached() {
        let card = CardId(7);
        let capture = sample_document(80, 50, 1, 0).source;
        let (mut worker, outcomes) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(
                        FrameEncoder::new()
                            .encode(&capture.frame, ImageFormat::Png)
                            .unwrap(),
                    ),
                    preview: None,
                },
                editor_source: None,
                rendered: None,
                provenance: capture.provenance,
                target: capture.target.clone(),
                scale: capture.frame.scale,
                color_space: capture.frame.color_space,
                capture_id: None,
            },
        );
        let data = Document::new(capture).data();
        worker.analyze_smart_frame(card, 3, 41, data.clone(), AnalysisCancellation::default());
        let first = outcomes
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("analysis result");
        assert!(matches!(
            first,
            Outcome::SmartFrameAnalyzed {
                card: CardId(7),
                generation: 3,
                revision: 41,
                result,
            } if result.is_ok()
        ));

        worker.analyze_smart_frame(card, 4, 42, data, AnalysisCancellation::default());
        let cached = received(&outcomes);
        assert!(matches!(
            cached,
            Outcome::SmartFrameAnalyzed {
                card: CardId(7),
                generation: 4,
                revision: 42,
                result,
            } if result.is_ok()
        ));
        assert_eq!(
            worker
                .analysis_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }

    #[test]
    fn cancelled_smart_frame_analysis_is_not_cached() {
        let card = CardId(8);
        let capture = sample_document(80, 50, 1, 0).source;
        let (mut worker, outcomes) = worker_holding(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(
                        FrameEncoder::new()
                            .encode(&capture.frame, ImageFormat::Png)
                            .unwrap(),
                    ),
                    preview: None,
                },
                editor_source: None,
                rendered: None,
                provenance: capture.provenance,
                target: capture.target.clone(),
                scale: capture.frame.scale,
                color_space: capture.frame.color_space,
                capture_id: None,
            },
        );
        let cancellation = AnalysisCancellation::default();
        cancellation.cancel();
        worker.analyze_smart_frame(card, 5, 99, Document::new(capture).data(), cancellation);

        assert!(matches!(
            received(&outcomes),
            Outcome::SmartFrameAnalyzed {
                generation: 5,
                revision: 99,
                result,
                ..
            } if matches!(result.as_ref(), Err(error) if error.contains("cancelled"))
        ));
        assert!(
            worker
                .analysis_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[test]
    fn a_pipeline_hands_out_distinct_card_identities() {
        let mut pipeline = start_pipeline();
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
        worker.run(&receiver, &PendingPinUpdates::default());

        let mut reopened = SqliteStore::open(dir.path()).expect("reopen history");
        assert!(reopened.image(&capture).unwrap().is_none());
        assert!(reopened.record(&capture).unwrap().is_some());
    }

    #[test]
    fn polling_an_idle_pipeline_does_not_block() {
        let pipeline = start_pipeline();
        assert!(pipeline.poll().is_none());
    }

    #[test]
    fn completed_worker_work_wakes_the_window_event_loop() {
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&wakes);
        let waker: SurfaceWaker = Arc::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        let pipeline =
            Pipeline::start_with_history_and_waker(Arc::new(RefusingSelector), false, Some(waker))
                .expect("worker");
        assert!(pipeline.post(Job::Copy(CardId(404))));
        let _ = wait_for(&pipeline);
        assert!(wakes.load(std::sync::atomic::Ordering::Relaxed) > 0);
    }

    #[test]
    fn queued_history_reads_coalesce_to_the_newest_generation() {
        let dir = scratch_dir("history-coalescing");
        let store = SqliteStore::open_ephemeral(dir.path()).expect("history store");
        let (outcomes, receiver) = channel();
        let (queries, query_receiver) = channel();
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&wakes);
        let waker: SurfaceWaker = Arc::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
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
                waker: Some(waker),
                store: Some(store),
                open_default: false,
            }
            .run(&query_receiver);
        });

        let Outcome::HistoryLoaded { request, .. } = received(&receiver) else {
            panic!("expected a history page");
        };
        assert_eq!(request, 2);
        assert!(wakes.load(std::sync::atomic::Ordering::Relaxed) > 0);
        queries.send(HistoryQuery::Stop).expect("stop history");
        worker.join().expect("history worker");
        drop(dir);
    }

    #[test]
    fn disabled_history_refuses_queries_without_opening_the_default_profile() {
        let pipeline =
            Pipeline::start_with_history(Arc::new(RefusingSelector), false).expect("worker");
        assert!(pipeline.query_history(7, SearchQuery::all()));

        let Some(Outcome::HistoryFailed {
            request,
            operation,
            error,
            ..
        }) = wait_for(&pipeline)
        else {
            panic!("disabled history did not answer explicitly");
        };
        assert_eq!(request, Some(7));
        assert_eq!(operation, HistoryOperation::Query);
        assert!(error.to_string().contains("unavailable"), "{error}");
    }

    #[test]
    fn a_stored_recording_stays_in_history_after_its_media_is_gone() {
        let (dir, mut worker, _receiver) = worker_with_store("worker-recording-row");
        let media = dir.path().join("Scrozz Recording.mp4");
        std::fs::write(&media, b"durable-video").expect("durable media");
        let canonical = std::fs::canonicalize(&media).expect("canonical media");

        let id = worker
            .store
            .as_mut()
            .expect("store")
            .insert_recording(
                scrozz_store::NewRecording::new(scrozz_store::VideoMetadata {
                    path: canonical.clone(),
                    duration_secs: 12.5,
                    engine: "test".to_owned(),
                    completion: scrozz_store::VideoCompletion::Complete,
                    size: Some(scrozz_core::PhysicalSize::new(1920.0, 1080.0)),
                    frames: Some(375),
                    audio_channels: Some(2),
                    file_size_bytes: Some(4_096),
                    codec: Some("h264".to_owned()),
                    content_type: Some("video/mp4".to_owned()),
                    quality: Some("balanced".to_owned()),
                    resolution: Some("native".to_owned()),
                })
                .from_app("Scrozz")
                .titled("Screen recording")
                .taken_at(scrozz_store::Timestamp(3_000)),
            )
            .expect("insert recording");

        // The full native summary must survive the sidecar round trip: history
        // is the only place a finished recording's encoder settings still exist
        // once the process ends, so a reduced type would lose them for good.
        let store = worker.store.as_mut().expect("store");
        let stored = store
            .record(&id)
            .expect("read")
            .expect("record exists")
            .video
            .expect("a recording row carries typed video metadata");
        assert_eq!(stored.path, canonical);
        assert!((stored.duration_secs - 12.5).abs() < f64::EPSILON);
        assert_eq!(stored.frames, Some(375));
        assert_eq!(stored.audio_channels, Some(2));
        assert_eq!(stored.file_size_bytes, Some(4_096));
        assert_eq!(stored.codec.as_deref(), Some("h264"));
        assert_eq!(stored.quality.as_deref(), Some("balanced"));
        assert_eq!(stored.resolution.as_deref(), Some("native"));

        let store = worker.store.as_mut().expect("store");
        let record = store.record(&id).expect("read").expect("record exists");
        let entry = history_entry(store, record)
            .expect("history entry")
            .expect("a recording is a history row, not a dropped one");
        assert_eq!(entry.media_kind, scrozz_store::MediaKind::Video);
        assert_eq!((entry.width, entry.height), (1920, 1080));
        assert!(entry.image_present, "the media file is still on disk");
        assert!(entry.content_error.is_none());
        assert!(
            entry.thumbnail.is_none(),
            "a recording has no editable raster to render a history preview from"
        );

        // Delete the media the way a user would. History never owned it, so the
        // row must survive and say what happened rather than disappearing.
        std::fs::remove_file(&canonical).expect("user removed the recording");
        let store = worker.store.as_mut().expect("store");
        let record = store.record(&id).expect("read").expect("record survives");
        let orphaned = history_entry(store, record)
            .expect("history entry")
            .expect("a recording row outlives its media");
        assert!(!orphaned.image_present);
        assert!(
            orphaned
                .content_error
                .as_deref()
                .is_some_and(|reason| reason.contains("no longer at")),
            "{:?}",
            orphaned.content_error
        );
        assert_eq!((orphaned.width, orphaned.height), (1920, 1080));
    }

    #[test]
    fn history_reads_return_filtered_entries_counts_apps_and_previews() {
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
        assert_eq!(page.apps, ["Figma", "Preview"]);
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].id, video_id);
        assert_eq!(page.entries[0].annotation_count, 2);
        assert!(page.entries[0].thumbnail.is_some());
    }

    #[test]
    fn one_unreadable_document_does_not_hide_the_rest_of_its_history_page() {
        let (dir, mut worker, _receiver) = worker_with_store("worker-future-document");
        let id = worker
            .store
            .as_mut()
            .expect("store")
            .insert(NewCapture::new(&sample_document(16, 8, 3, 1)))
            .expect("insert");
        let record = worker
            .store
            .as_ref()
            .expect("store")
            .record(&id)
            .expect("read")
            .expect("record");
        let path = worker
            .store
            .as_ref()
            .expect("store")
            .layout()
            .record_path(&id)
            .expect("record path");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read sidecar"))
                .expect("decode sidecar");
        value["document"] = serde_json::json!({"future_annotation_schema": 99});
        std::fs::write(&path, serde_json::to_vec(&value).expect("encode sidecar"))
            .expect("write sidecar");

        let entry = history_entry(worker.store.as_mut().expect("store"), record)
            .expect("page remains readable")
            .expect("entry remains visible");
        assert_eq!(entry.id, id);
        assert!(entry.thumbnail.is_none());
        assert!(entry.content_error.is_some());
        drop(dir);
    }

    #[test]
    fn history_restore_and_editor_keep_the_complete_document() {
        let (_dir, mut worker, receiver) = worker_with_store("worker-restore");
        let document = richly_annotated_document(42);
        let id = worker
            .store
            .as_mut()
            .expect("store")
            .insert(NewCapture::new(&document).titled("Editable"))
            .expect("insert");

        worker.open_history_editor(id.clone(), CardId(8));
        let Outcome::Opened {
            card,
            document: loaded,
            editor_only,
        } = received(&receiver)
        else {
            panic!("expected an editable document");
        };
        assert_eq!(card, CardId(8));
        assert!(editor_only);
        assert_eq!(loaded.data(), document.data());
        assert!(worker.vault.get(CardId(8)).is_some());
        assert!(matches!(
            received(&receiver),
            Outcome::HistoryDone {
                operation: HistoryOperation::OpenEditor,
                ..
            }
        ));

        let expected = SkiaRenderer::new()
            .render(&document)
            .expect("reference render");
        worker.prepare_image(CardId(8), 1, 7, document.data());
        assert!(matches!(
            received(&receiver),
            Outcome::Prepared {
                card: CardId(8),
                generation: 1,
                revision: 7,
            }
        ));
        let prepared = worker
            .vault
            .get_revision(CardId(8), 1, 7)
            .expect("prepared history revision");
        let actual = scrozz_export::decode(&prepared.full).expect("decode prepared revision");
        assert_eq!(
            actual.data, expected.data,
            "stored annotations were baked into the source and applied twice"
        );

        worker.restore(id.clone(), CardId(9));
        let Outcome::Restored(card) = received(&receiver) else {
            panic!("expected a restored card");
        };
        assert_eq!(card.id, CardId(9));
        assert_eq!(card.capture_id.as_ref(), Some(&id));
        assert_eq!(card.source_px(), (128, 112));
        assert!(worker.vault.get(CardId(9)).is_some());
    }

    #[test]
    fn selected_history_drag_is_fully_rendered_before_the_gesture() {
        let (dir, mut worker, receiver) = worker_with_store("worker-drag-preload");
        let id = worker
            .store
            .as_mut()
            .expect("store")
            .insert(NewCapture::new(&richly_annotated_document(5)))
            .expect("insert");

        worker.prepare_history_drag(id.clone());
        let Outcome::HistoryDragPrepared { capture, prepared } = received(&receiver) else {
            panic!("expected a prepared history drag");
        };
        assert_eq!(capture, id);
        let geometry = DragGeometry {
            rect: LogicalRect::new(
                LogicalPoint::new(10.0, 20.0),
                scrozz_core::LogicalSize::new(240.0, 160.0),
            ),
            pointer: LogicalPoint::new(80.0, 60.0),
        };
        let payload = prepared.payload(geometry).expect("payload");
        let (artifact, bytes) = payload.materialise(dir.path()).expect("materialise");
        assert!(!bytes.is_empty());
        drop(artifact);
    }

    #[test]
    fn history_pin_and_delete_mutate_the_worker_store() {
        let (_dir, mut worker, receiver) = worker_with_store("worker-mutations");
        let id = worker
            .store
            .as_mut()
            .expect("store")
            .insert(NewCapture::new(&sample_document(16, 8, 3, 1)))
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
    fn persisted_pin_texture_renders_saved_edits_instead_of_raw_source_pixels() {
        let (_dir, mut worker, _receiver) = worker_with_store("worker-pin-render");
        let document = richly_annotated_document(17);
        let id = worker
            .store
            .as_mut()
            .expect("store")
            .insert(NewCapture::new(&document))
            .expect("insert");

        let actual = worker.load_pin_texture(&id).expect("pin texture");
        let rendered = SkiaRenderer::new()
            .render(&document)
            .expect("render saved edits");
        let expected =
            Thumbnail::from_frame(&rendered, PIN_TEXTURE_MAX_EDGE).expect("expected texture");
        let raw = Thumbnail::from_frame(&document.source.frame, PIN_TEXTURE_MAX_EDGE)
            .expect("raw source texture");

        assert_eq!(actual.pixels(), expected.pixels());
        assert_ne!(
            actual.pixels(),
            raw.pixels(),
            "pinning exposed the pixels underneath the saved edits"
        );
    }

    #[test]
    fn pinning_an_active_editor_persists_and_displays_the_exact_revision() {
        let (_dir, mut worker, receiver) = worker_with_store("worker-edited-pin");
        let original = richly_annotated_document(4);
        let edited = richly_annotated_document(44);
        let id = worker
            .store
            .as_mut()
            .expect("store")
            .insert(NewCapture::new(&original))
            .expect("insert");
        let card = CardId(13);
        worker.open_history_editor(id.clone(), card);
        let _ = received(&receiver);
        let _ = received(&receiver);
        let rendered = RevisionedFrame::from_document(&edited, 8).expect("render edited pin");
        let expected =
            Thumbnail::from_frame(rendered.frame(), PIN_TEXTURE_MAX_EDGE).expect("pin texture");
        let editor = PinEditorSnapshot {
            generation: 5,
            revision: 8,
            rendered,
            document: edited.data(),
        };
        let state = PinState::new(
            LogicalRect::new(
                LogicalPoint::new(10.0, 20.0),
                scrozz_core::LogicalSize::new(320.0, 180.0),
            ),
            scrozz_core::PinScale::ORIGINAL,
            None,
        );

        worker.pin_card(card, &id, PinGeneration(1), &state, Some(&editor));

        let Outcome::PinCreated { texture, .. } = received(&receiver) else {
            panic!("edited pin was not committed");
        };
        assert_eq!(
            texture.expect("edited pin texture").pixels(),
            expected.pixels()
        );
        let DocumentState::Complete(reloaded) = worker
            .store
            .as_mut()
            .expect("store")
            .document(&id)
            .expect("read")
            .expect("document")
        else {
            panic!("edited pin document was evicted");
        };
        assert_eq!(reloaded.data(), edited.data());
    }

    #[test]
    fn closing_a_history_editor_persists_its_exact_scene_graph() {
        let (_dir, mut worker, receiver) = worker_with_store("worker-editor-persist");
        let original = richly_annotated_document(1);
        let changed = richly_annotated_document(99);
        let id = worker
            .store
            .as_mut()
            .expect("store")
            .insert(NewCapture::new(&original))
            .expect("insert");
        worker.open_history_editor(id.clone(), CardId(12));
        let _ = received(&receiver);
        let _ = received(&receiver);

        worker.persist_document(CardId(12), 3, 9, &changed.data());

        assert!(matches!(
            received(&receiver),
            Outcome::HistoryDone {
                operation: HistoryOperation::Edit,
                capture: Some(ref saved),
                ..
            } if saved == &id
        ));
        let DocumentState::Complete(reloaded) = worker
            .store
            .as_mut()
            .expect("store")
            .document(&id)
            .expect("read")
            .expect("document")
        else {
            panic!("saved editor source was unexpectedly evicted");
        };
        assert_eq!(reloaded.data(), changed.data());
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

    #[test]
    fn selector_outputs_receive_durable_identity_and_pin_ready_provenance() {
        let root = scratch("selector-output");
        let store = SqliteStore::open_ephemeral(&root).expect("ephemeral history");
        let (outcomes, _outcome_rx) = channel();
        let vault = CaptureVault::new();
        let mut worker = Worker {
            outcomes,
            waker: None,
            selector: Arc::new(RefusingSelector),
            store: Some(store),
            vault,
            pin_generations: HashMap::new(),
            retention_policy: RetentionPolicy::default(),
            derived_documents: HashMap::new(),
            analysis_cache: Arc::new(Mutex::new(HashMap::new())),
        };

        let card = worker
            .finish_capture(
                CaptureKind::Window,
                CardId(44),
                sample_capture(Provenance::Window),
                &no_after_capture_actions(),
            )
            .expect("selector output enters card pipeline")
            .card;

        assert!(
            card.capture_id.is_some(),
            "Pin to Screen needs durable identity"
        );
        assert_eq!(card.provenance, Provenance::Window);
        assert!(
            card.thumbnail.is_some(),
            "the pin must begin with real pixels"
        );
        drop(worker);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_persisted_dimensions_use_a_bounded_error_viewport() {
        let mut header = FrameHeader::of(&sample_capture(Provenance::Window).frame);
        header.size = PhysicalSize::new(f64::NAN, f64::INFINITY);

        let (width, height, scale, error) = safe_pin_source_geometry(&header);
        assert_eq!((width, height, scale), (420, 180, 1.0));
        assert!(error.is_some());
    }

    #[test]
    fn missing_pin_pixels_surface_an_error_without_clearing_recoverable_state() {
        let root = scratch("missing-pin-pixels");
        let store = SqliteStore::open_ephemeral(&root).expect("ephemeral history");
        let (outcomes, _outcome_rx) = channel();
        let vault = CaptureVault::new();
        let mut worker = Worker {
            outcomes,
            waker: None,
            selector: Arc::new(RefusingSelector),
            store: Some(store),
            vault,
            pin_generations: HashMap::new(),
            retention_policy: RetentionPolicy::default(),
            derived_documents: HashMap::new(),
            analysis_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        let card = worker
            .finish_capture(
                CaptureKind::Window,
                CardId(45),
                sample_capture(Provenance::Window),
                &no_after_capture_actions(),
            )
            .expect("stored capture")
            .card;
        let id = card.capture_id.expect("durable identity");
        let state = PinState::new(
            scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(10.0, 20.0),
                scrozz_core::LogicalSize::new(320.0, 180.0),
            ),
            scrozz_core::PinScale::ORIGINAL,
            None,
        );
        let store = worker.store.as_mut().expect("store");
        store
            .set_screen_pin(&id, Some(&state))
            .expect("persist pin state");
        let record = store.record(&id).expect("record read").expect("record");
        let scrozz_store::ImageState::Present { hash, .. } = record.image else {
            panic!("new capture pixels must be present");
        };
        let blob = store.layout().blob_path(&hash).expect("blob path");
        std::fs::remove_file(blob).expect("simulate unreadable pixels");

        let restored = worker.load_pin(&id).expect("pin metadata survives");
        assert!(restored.texture.is_none());
        assert!(
            restored
                .content_error
                .as_deref()
                .is_some_and(|error| error.contains("pixels"))
        );
        assert_eq!(
            worker
                .store
                .as_ref()
                .expect("store")
                .record(&id)
                .expect("record")
                .expect("present")
                .screen_pin,
            Some(state)
        );
        drop(worker);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_initial_pin_write_failure_requests_visible_rollback() {
        let (outcomes, outcome_rx) = channel();
        let card = CardId(9);
        let capture = CaptureId("capture-9".into());
        let vault = CaptureVault::new();
        vault.store(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::new(Vec::new()),
                    preview: None,
                },
                editor_source: None,
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::AllDisplays,
                scale: ScaleFactor::IDENTITY,
                color_space: ColorSpace::Srgb,
                capture_id: Some(capture.clone()),
            },
        );
        let mut worker = Worker {
            outcomes,
            waker: None,
            selector: Arc::new(RefusingSelector),
            store: None,
            vault,
            pin_generations: HashMap::new(),
            retention_policy: RetentionPolicy::default(),
            derived_documents: HashMap::new(),
            analysis_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        let state = PinState::new(
            scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(10.0, 20.0),
                scrozz_core::LogicalSize::new(320.0, 180.0),
            ),
            scrozz_core::PinScale::ORIGINAL,
            None,
        );

        worker.pin_card(card, &capture, PinGeneration(1), &state, None);

        match outcome_rx.recv().expect("rollback outcome") {
            Outcome::PinCreationFailed {
                capture: failed,
                generation,
                error,
            } => {
                assert_eq!(failed, capture);
                assert_eq!(generation, PinGeneration(1));
                assert!(error.to_string().contains("history is unavailable"));
            }
            other => panic!("expected pin creation failure, got {other:?}"),
        }
        assert!(
            worker.vault.get(card).is_some(),
            "the source card stays fully usable after rollback"
        );
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

        assert!(pending.queue(capture.clone(), PinGeneration(1), Some(first)));
        assert!(!pending.queue(capture.clone(), PinGeneration(1), Some(latest.clone())));

        let updates = pending.take();
        assert_eq!(updates.len(), 1);
        assert_eq!(
            updates.get(&capture),
            Some(&PendingPinUpdate {
                generation: PinGeneration(1),
                state: Some(latest),
            })
        );
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

        assert!(pending.queue(capture.clone(), PinGeneration(1), Some(state)));
        pending.cancel_through(&capture, PinGeneration(2));
        assert!(pending.take().is_empty());
    }

    #[test]
    fn stale_updates_cannot_replace_a_newer_pending_generation() {
        let pending = PendingPinUpdates::default();
        let capture = CaptureId("capture-repinned".into());
        let newer = PinState::new(
            scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(50.0, 60.0),
                scrozz_core::LogicalSize::new(320.0, 180.0),
            ),
            scrozz_core::PinScale::ORIGINAL,
            None,
        );
        let mut stale = newer.clone();
        stale.frame.origin.x = 1.0;

        assert!(pending.queue(capture.clone(), PinGeneration(3), Some(newer.clone())));
        assert!(!pending.queue(capture.clone(), PinGeneration(2), Some(stale)));
        pending.cancel_through(&capture, PinGeneration(2));

        assert_eq!(
            pending.take().get(&capture),
            Some(&PendingPinUpdate {
                generation: PinGeneration(3),
                state: Some(newer),
            })
        );
    }

    #[test]
    fn a_stale_terminal_settlement_cannot_override_a_new_pin() {
        let (outcomes, _outcome_rx) = channel();
        let capture = CaptureId("capture-generation".into());
        let mut worker = Worker {
            outcomes,
            waker: None,
            selector: Arc::new(RefusingSelector),
            store: None,
            vault: CaptureVault::new(),
            pin_generations: HashMap::new(),
            retention_policy: RetentionPolicy::default(),
            derived_documents: HashMap::new(),
            analysis_cache: Arc::new(Mutex::new(HashMap::new())),
        };

        assert!(worker.claim_pin_generation(&capture, PinGeneration(3)));
        assert!(!worker.claim_pin_generation(&capture, PinGeneration(2)));
        assert!(worker.claim_pin_generation(&capture, PinGeneration(3)));
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
