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
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    thread::JoinHandle,
    time::{Duration, SystemTime},
};

use scrozz_annotate::{
    AnalysisCancellation, Document, DocumentData, Renderer, SkiaRenderer, SmartFrameAnalysis,
    analyze_smart_frame,
};
use scrozz_core::{
    Capture, CaptureRequest, CaptureTarget, ColorSpace, CursorMode, Error as CoreError,
    LogicalPoint, LogicalRect, PinState, Provenance, ScaleFactor, ScrollAxis, SelectionMode,
    SelectionOptions,
};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, RgbaImage};
use scrozz_shell::{DragPayload, DragPreview, byte_source};
use scrozz_stitch::{AtomicCancellation, CancelAction, Progress};
use scrozz_store::{
    CaptureId, CaptureRecord, CaptureSharing, DocumentState, FrameHeader, History, ImageState,
    NewCapture, Page, RemoteObjectId, RetentionPolicy, SearchQuery, ShareProvider, ShareTag,
    ShareUrl, SharedMediaKind, SqliteStore, Store, Timestamp,
};
use scrozz_ui::editor::RevisionedFrame;
use scrozz_ui::history::{HistoryEntry, HistoryPage, HistoryThumbnail};

use crate::{
    after_capture::{
        ActionEffect, ActionExecutor, AfterCaptureAction, AfterCaptureSettings, ExecutionReport,
        FinalizedScreenshot, MediaKind, orchestrate,
    },
    commands::ScrollingTarget,
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

/// How long `stop` waits for the worker to answer before reporting that it is
/// still busy.
///
/// A scrolling session can be mid-gesture when the app quits; the worker is
/// asked to cancel and then given a bounded moment to unwind, rather than the
/// UI thread blocking on a join that a stalled compositor could hold forever.
const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// The strict ceiling on full-resolution frames the capture worker may hold.
///
/// A forwarded `scrozz capture` hands over whole uncompressed frames. Without a
/// ceiling a burst of terminal invocations would grow the job queue — and
/// therefore resident memory — without limit, because the worker is slower than
/// the socket. Two is one being processed plus one waiting.
const MAX_QUEUED_CAPTURE_FRAMES: usize = 2;

#[derive(Clone)]
struct CaptureBudget {
    in_flight: Arc<AtomicUsize>,
}

impl CaptureBudget {
    fn new() -> Self {
        Self {
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn try_acquire(&self) -> Option<CapturePermit> {
        self.in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < MAX_QUEUED_CAPTURE_FRAMES).then_some(current + 1)
            })
            .ok()
            .map(|_| CapturePermit {
                in_flight: Arc::clone(&self.in_flight),
            })
    }

    fn has_capacity(&self) -> bool {
        self.in_flight.load(Ordering::Acquire) < MAX_QUEUED_CAPTURE_FRAMES
    }
}

/// Proof that one admitted frame is accounted for.
///
/// Held by the queued job, so the budget is released exactly when the worker
/// drops the job — including when the worker exits with jobs still queued.
#[derive(Debug)]
pub(crate) struct CapturePermit {
    in_flight: Arc<AtomicUsize>,
}

impl Drop for CapturePermit {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

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
pub(crate) enum Job {
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
    /// File a committed editor revision into the card's own exportable bytes.
    ///
    /// Posted once, alongside [`Job::PersistDocument`], when Done commits a
    /// dirty document — never for a cancelled or discarded one. After this a
    /// plain [`Job::Copy`], [`Job::Save`] or [`Job::Upload`] — posted once no
    /// editor is open — reads this exact committed revision instead of the
    /// pre-edit original, so a destructive redaction the card's thumbnail
    /// already shows committed cannot resurface through a card output that
    /// outlives the editor that applied it.
    CommitCardOutput {
        /// Which card's own bytes to replace.
        card: CardId,
        /// Editor lifetime that produced the render.
        generation: u64,
        /// The flattened image and the exact document revision it represents.
        rendered: Box<RevisionedFrame>,
        /// The same document the render came from.
        ///
        /// Kept alongside the pixels so the worker can also refresh its
        /// in-memory reopen document in the same step -- [`Job::PersistDocument`]
        /// writes the durable history copy separately and can fail or lag
        /// independently, and a later [`Job::Open`] must never reconstruct an
        /// older document than the one this job just committed here.
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
        /// Admission token that strictly bounds full-resolution frames waiting
        /// for or being processed by the worker.
        _permit: CapturePermit,
    },
    /// Take and assemble repeated frames along one explicit axis.
    ///
    /// Separate from [`Job::Capture`] because it is the only capture that runs
    /// for as long as the user keeps it running, reports progress while it does,
    /// and can be ended two different ways.
    Scrolling {
        /// Direction the stitched image grows.
        axis: ScrollAxis,
        /// Identity reserved for the resulting card.
        card: CardId,
        /// Where the request entered the app.
        origin: CaptureOrigin,
        /// The persisted GUI policy snapshotted when this capture was requested.
        policy: AfterCaptureSettings,
        /// Window identity selected before the HUD and geometry refreshed when
        /// the user chose an axis.
        target: Box<ScrollingTarget>,
        /// One-way cancellation for queued or in-flight frame acquisition.
        acquisition_cancellation: scrozz_capture::CaptureCancellation,
    },
    /// Put a card's capture on the clipboard.
    Copy {
        /// Card whose immutable bytes are being copied.
        card: CardId,
        /// The dispatching action id this completion answers for (round 12).
        /// See [`Job::Upload::action`] for why every output family, not
        /// just Upload, now carries one: a card-level dispatch racing an
        /// editor's own in-editor Copy/Save for the same card must resolve
        /// only the exact action it belongs to, never any other
        /// concurrently outstanding one.
        action: u64,
    },
    /// Put an already-rendered image on the clipboard.
    ///
    /// Used by the editor, which has flattened its own annotations and must not
    /// have the card's unannotated capture substituted for them.
    CopyImage {
        /// Which card the image came from, for the log line.
        card: CardId,
        /// Editor lifetime that produced the render, so the completion can be
        /// matched against the card's currently-committed revision rather
        /// than trusted blindly (round 5, Finding #2).
        generation: u64,
        /// The flattened image and the exact document revision it represents.
        rendered: Box<RevisionedFrame>,
        /// See [`Job::Copy::action`].
        action: u64,
    },
    /// Write an already-rendered image to the configured folder.
    SaveImage {
        /// Which card the image came from, for the log line.
        card: CardId,
        /// Editor lifetime that produced the render, so the completion can be
        /// matched against the card's currently-committed revision rather
        /// than trusted blindly (round 5, Finding #2).
        generation: u64,
        /// The flattened image and the exact document revision it represents.
        rendered: Box<RevisionedFrame>,
        /// See [`Job::Copy::action`].
        action: u64,
    },
    /// Write an already-rendered image to an explicitly chosen path.
    SaveImageTo {
        /// Which card the image came from, for the log line.
        card: CardId,
        /// Editor lifetime that produced the render, so the completion can be
        /// matched against the card's currently-committed revision rather
        /// than trusted blindly (round 5, Finding #2).
        generation: u64,
        /// The flattened image and the exact document revision it represents.
        rendered: Box<RevisionedFrame>,
        /// Native-dialog destination.
        path: std::path::PathBuf,
        /// See [`Job::Copy::action`].
        action: u64,
    },
    /// Write a card's capture to the configured folder.
    Save {
        /// Card whose immutable bytes are being exported.
        card: CardId,
        /// See [`Job::Copy::action`].
        action: u64,
    },
    /// Write a card's capture to an explicitly chosen path.
    SaveTo {
        /// Card whose immutable bytes are being exported.
        card: CardId,
        /// Native-dialog destination.
        path: std::path::PathBuf,
        /// See [`Job::Copy::action`].
        action: u64,
    },
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
    /// Upload a card's untouched capture and copy the returned link.
    Upload {
        /// Which card.
        card: CardId,
        /// The dispatching action id this upload answers for (round 7,
        /// Finding #2). Travels back on `Outcome::UploadDone`/
        /// `Outcome::UploadRefused` so a completion for a since-superseded
        /// Upload request can be told apart from the card's current one.
        action: u64,
    },
    /// Upload an already-rendered editor revision instead of the original.
    ///
    /// The counterpart of [`Job::CopyImage`] and [`Job::SaveImage`], and it
    /// exists for the same reason: an edited card must never fall back to the
    /// pixels a destructive redaction removed.
    UploadImage {
        /// Which card the image came from.
        card: CardId,
        /// Editor lifetime that produced the render.
        generation: u64,
        /// The flattened image and the exact document revision it represents.
        rendered: Box<RevisionedFrame>,
        /// See [`Job::Upload::action`].
        action: u64,
    },
    /// Upload the durable media a recording card is showing.
    UploadRecording {
        /// Card the recording belongs to.
        card: CardId,
        /// Durable history identity, when the recording was remembered.
        capture: Option<CaptureId>,
        /// Canonical file written by the recorder or the video editor.
        path: std::path::PathBuf,
        /// IANA type matching the durable file and codec.
        content_type: String,
        /// Safe file name for the remote object.
        file_name: String,
        /// See [`Job::Upload::action`].
        action: u64,
    },
    /// Persist successful remote metadata on the store-owning worker.
    RememberShare {
        /// Durable capture the share belongs to.
        capture_id: CaptureId,
        /// Secret-free remote metadata.
        sharing: RememberedShare,
    },
    /// Forget a card's cached bytes. The card itself is the surface's business.
    Release(CardId),
    /// Release live bytes only if durable history still owns the source pixels.
    ReleaseIfRetained {
        /// Live card awaiting a safe cleanup decision.
        card: CardId,
        /// Durable history identity to inspect.
        capture: CaptureId,
    },
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

/// Secret-safe handoff from the upload worker to history.
#[derive(Clone)]
pub(crate) struct RememberedShare {
    url: String,
    key: String,
    provider: &'static str,
    expires_at: Option<SystemTime>,
    encrypted: bool,
    tags: Vec<(String, String)>,
    media_kind: crate::cloud::ArtifactKind,
}

impl std::fmt::Debug for RememberedShare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RememberedShare")
            .field("url", &"[BEARER URL REDACTED]")
            .field("key", &self.key)
            .field("provider", &self.provider)
            .field("expires_at", &self.expires_at)
            .field("encrypted", &self.encrypted)
            .field("tags", &self.tags)
            .field("media_kind", &self.media_kind)
            .finish()
    }
}

impl RememberedShare {
    fn from_shared(shared: &crate::cloud::Shared) -> Self {
        Self {
            url: shared.url.clone(),
            key: shared.key.clone(),
            provider: shared.provider,
            expires_at: shared.expires_at,
            encrypted: shared.encrypted,
            tags: shared.tags.clone(),
            media_kind: shared.media_kind,
        }
    }

    fn into_history(self) -> scrozz_core::Result<CaptureSharing> {
        let provider = match self.provider {
            "aws" => ShareProvider::Aws,
            "r2" => ShareProvider::R2,
            "b2" => ShareProvider::B2,
            "minio" => ShareProvider::Minio,
            other => ShareProvider::Unknown(other.to_owned()),
        };
        let media_kind = if self.encrypted {
            SharedMediaKind::ViewerPage
        } else {
            match self.media_kind {
                crate::cloud::ArtifactKind::Screenshot => SharedMediaKind::Image,
                crate::cloud::ArtifactKind::Recording => {
                    SharedMediaKind::Unknown("video".to_owned())
                }
            }
        };
        let mut sharing = CaptureSharing::new(
            ShareUrl::new(self.url)?,
            provider,
            RemoteObjectId::new(self.key)?,
            media_kind,
        )
        .tagged(
            self.tags
                .into_iter()
                .map(|(key, value)| ShareTag::new(key, value))
                .collect::<scrozz_core::Result<Vec<_>>>()?,
        );
        if let Some(expires_at) = self.expires_at {
            sharing = sharing.expiring_at(Timestamp::from_system_time(expires_at));
        }
        Ok(sharing)
    }
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
    /// A slower card action was accepted by its dedicated worker.
    Started {
        /// Which card.
        card: CardId,
        /// What began.
        detail: String,
    },
    /// A card action completed, with a phrase for the log.
    Done {
        /// Which card.
        card: CardId,
        /// What happened, e.g. "copied to the clipboard".
        detail: String,
        /// The editor generation and document revision the completed output
        /// was rendered from, when a live editor produced it. `None` when
        /// the output was read from the card's own cache with no editor
        /// involved. Lets the main thread refuse to mark a *newer*,
        /// since-committed revision as retained/exported just because an
        /// older revision's save happened to finish after Done moved the
        /// card on (round 5, Finding #2).
        version: Option<(u64, u64)>,
        /// The dispatching action id this completion answers for (round 12).
        /// See [`Job::Copy::action`]: a card-level Copy/Save/Save-As can
        /// now be concurrently outstanding alongside an editor's own
        /// in-editor Copy/Save (or another card-level dispatch of a
        /// different kind) for the same card, so `card` alone no longer
        /// identifies which dispatch this answers for.
        action: u64,
    },
    /// A cloud upload completed and its share link reached the clipboard.
    UploadDone {
        /// Which card.
        card: CardId,
        /// User-facing completion detail.
        detail: String,
        /// The editor generation and document revision the uploaded image
        /// was rendered from, when a live editor produced it. See
        /// [`Outcome::Done::version`].
        version: Option<(u64, u64)>,
        /// The dispatching action id this completion answers for (round 7,
        /// Finding #2). A second Upload request can be dispatched for the
        /// same card before an earlier one's outcome is drained; comparing
        /// this against the card's *current* action lets the main thread
        /// tell the two apart and ignore a stale answer instead of acting on
        /// bookkeeping (`close_after_upload`/`overflow_recovery_in_flight`)
        /// that now belongs to the newer request.
        action: u64,
    },
    /// A card action failed.
    Refused {
        /// Which card.
        card: CardId,
        /// Why.
        error: CliError,
    },
    /// A cloud upload failed before its share link reached the clipboard.
    UploadRefused {
        /// Which card.
        card: CardId,
        /// Why.
        error: CliError,
        /// See [`Outcome::UploadDone::action`].
        action: u64,
    },
    /// A requested card output failed.
    OutputRefused {
        /// Which card.
        card: CardId,
        /// Why.
        error: CliError,
        /// See [`Outcome::Done::action`].
        action: u64,
    },
    /// Result of an atomic durable-source check and live-byte release.
    RetentionRelease {
        /// Live card awaiting the decision.
        card: CardId,
        /// Whether live bytes were released because history owned the source.
        released: bool,
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
    /// A Done exit's card-output commit -- the bytes a plain Copy, Save,
    /// Upload or drag reads once the editor is gone -- has been durably
    /// filed, for the exact editor generation and document revision it was
    /// posted for.
    ///
    /// Finding #1 (round 5): the window this belongs to must not finalize
    /// closed, and no output on this card may read its own bytes, until this
    /// arrives (or [`Self::CardOutputCommitFailed`] does): posting the job
    /// is not the same as it having landed, and a delayed or refused commit
    /// must never let a plain export still see the pre-edit pixels the
    /// thumbnail already told the user were replaced.
    CardOutputCommitted {
        /// Which card.
        card: CardId,
        /// Which opening of the editor produced the commit.
        generation: u64,
        /// The document revision now filed as the card's own bytes.
        revision: u64,
    },
    /// A Done exit's card-output commit could not be filed (e.g. the card
    /// had already left the vault, or the revision could not be encoded).
    ///
    /// The editor this was posted for must not finalize closed: it stays
    /// open (or reopens, if the native viewport already closed for this
    /// frame) with the failure surfaced, exactly as if `render` itself had
    /// failed -- never silently treated as if Done had succeeded.
    CardOutputCommitFailed {
        /// Which card.
        card: CardId,
        /// Which opening of the editor attempted the commit.
        generation: u64,
        /// The document revision that failed to file.
        revision: u64,
        /// Why it failed.
        error: CliError,
    },
    /// A Done exit's (or a history-only editor's) scene graph has been
    /// durably persisted to capture history, for the exact editor
    /// generation and document revision it was posted for.
    ///
    /// Emitted in addition to (after) [`Self::HistoryDone`], which continues
    /// to drive only the History browser panel. This variant exists so an
    /// editor's close can be gated on the exact write it is waiting for
    /// without depending on `HistoryDone`'s broader, multi-operation shape,
    /// or on it firing for every kind of history mutation.
    EditorClosePersisted {
        /// Which card.
        card: CardId,
        /// Which opening of the editor produced the write.
        generation: u64,
        /// The document revision now durably persisted.
        revision: u64,
    },
    /// A Done exit's (or a history-only editor's) scene graph could not be
    /// persisted (Finding #4, round 5).
    ///
    /// The editor this was posted for must not release its editor-only
    /// cache, nor finalize closed: an unconditional release right after a
    /// failed persist would discard the exact edit the write was trying to
    /// save. The editor instead stays open (or reopens) with the failure
    /// surfaced.
    EditorClosePersistFailed {
        /// Which card.
        card: CardId,
        /// Which opening of the editor attempted the write.
        generation: u64,
        /// The document revision that failed to persist.
        revision: u64,
        /// Why it failed.
        error: CliError,
    },
}

/// A finalized card plus every isolated ambient action result.
#[derive(Debug)]
pub struct ReadyCapture {
    /// The surface model built from the immutable artifact.
    pub card: Card,
    /// Enabled actions in deterministic execution order.
    pub actions: ExecutionReport,
    /// Whether a durable history source or confirmed external destination owns the artifact.
    pub retained_elsewhere: bool,
    /// Whether After Capture already wrote the artifact to a local export path.
    pub exported: bool,
}

/// A handle to the capture and history threads.
pub struct Pipeline {
    jobs: Sender<Job>,
    capture_budget: CaptureBudget,
    /// Signalled when the capture worker's loop has actually returned.
    worker_done: Receiver<()>,
    /// Keep/abort for the scrolling session the worker is running.
    scrolling_cancellation: AtomicCancellation,
    /// The acquisition token of that session, so Abort also unblocks a frame
    /// wait inside the native backend rather than only the stitch loop.
    active_scrolling_acquisition: Mutex<Option<scrozz_capture::CaptureCancellation>>,
    pending_pin_updates: Arc<PendingPinUpdates>,
    history_queries: Sender<HistoryQuery>,
    uploads: Sender<UploadJob>,
    outcomes: Receiver<Outcome>,
    /// A clone of the outcome sender, kept only so tests can inject an
    /// outcome the real workers cannot produce hermetically — an uploaded
    /// share, which needs a network and (without the `cloud` feature) a
    /// backend that is not compiled in at all. Every other job's outcome is
    /// exercised by actually posting the job and draining the real worker.
    #[cfg(test)]
    test_outcomes: Sender<Outcome>,
    worker: Option<JoinHandle<()>>,
    history_worker: Option<JoinHandle<()>>,
    upload_worker: Option<JoinHandle<()>>,
    upload_cancellation: crate::cloud::ShareCancellation,
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
        let (worker_done_tx, worker_done) = channel();
        let (history_queries, history_rx) = channel();
        let (uploads, upload_rx) = channel();
        let (outcome_tx, outcomes) = channel();
        let vault = CaptureVault::new();
        let worker_vault = vault.clone();
        let pending_pin_updates = Arc::new(PendingPinUpdates::default());
        let capture_budget = CaptureBudget::new();
        let worker_pin_updates = Arc::clone(&pending_pin_updates);
        #[cfg(test)]
        let test_outcomes = outcome_tx.clone();
        let history_outcomes = outcome_tx.clone();
        let history_waker = waker.clone();
        let upload_cancellation = crate::cloud::ShareCancellation::default();
        let upload_cancel = upload_cancellation.clone();
        let upload_outcomes = outcome_tx.clone();
        let upload_waker = waker.clone();
        let upload_history = jobs.clone();

        // Uploading is the one job that can block on a remote host for minutes,
        // so it gets a worker of its own: a stalled or failing share must never
        // hold up the shutter, the clipboard, or a history read. It is started
        // first and stopped last, because it is the only worker that posts work
        // back to the store-owning capture worker.
        let upload_worker = std::thread::Builder::new()
            .name("scrozz-upload".to_owned())
            .spawn(move || {
                UploadWorker::new(upload_outcomes, upload_waker, upload_cancel, upload_history)
                    .run(&upload_rx);
            })
            .map_err(|err| {
                CliError::Core(CoreError::Platform(format!(
                    "could not start the upload worker: {err}"
                )))
            })?;

        let capture_uploads = uploads.clone();
        let scrolling_cancellation = AtomicCancellation::default();
        let worker_scrolling_cancellation = scrolling_cancellation.clone();
        let worker = match std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || {
                Worker::new(
                    outcome_tx,
                    capture_uploads,
                    selector,
                    worker_vault,
                    history_enabled,
                    waker,
                    retention_policy,
                )
                .with_scrolling_cancellation(worker_scrolling_cancellation)
                .run(&job_rx, &worker_pin_updates);
                // Sent from inside the thread so `stop` can distinguish "the
                // loop returned" from "the join is blocked on native work".
                let _ = worker_done_tx.send(());
            }) {
            Ok(worker) => worker,
            Err(err) => {
                upload_cancellation.cancel();
                let _ = uploads.send(UploadJob::Stop);
                let _ = upload_worker.join();
                return Err(CliError::Core(CoreError::Platform(format!(
                    "could not start the capture worker: {err}"
                ))));
            }
        };

        let history_worker = match std::thread::Builder::new()
            .name("scrozz-history".to_owned())
            .spawn(move || {
                HistoryReader::new(history_outcomes, history_waker, history_enabled)
                    .run(&history_rx);
            }) {
            Ok(worker) => worker,
            Err(err) => {
                upload_cancellation.cancel();
                let _ = uploads.send(UploadJob::Stop);
                let _ = upload_worker.join();
                let _ = jobs.send(Job::Stop);
                let _ = worker.join();
                return Err(CliError::Core(CoreError::Platform(format!(
                    "could not start the history worker: {err}"
                ))));
            }
        };

        Ok(Self {
            jobs,
            capture_budget,
            worker_done,
            scrolling_cancellation,
            active_scrolling_acquisition: Mutex::new(None),
            pending_pin_updates,
            history_queries,
            uploads,
            outcomes,
            #[cfg(test)]
            test_outcomes,
            worker: Some(worker),
            history_worker: Some(history_worker),
            upload_worker: Some(upload_worker),
            upload_cancellation,
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

    /// Whether one forwarded full-resolution frame can be admitted now.
    #[must_use]
    pub fn can_accept_forwarded_capture(&self) -> bool {
        self.capture_budget.has_capacity()
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

    /// Feed completed external capture pixels into the durable card pipeline.
    ///
    /// Region/window selectors and forwarded CLI captures own their capture
    /// lifecycle, but their output must still receive history identity, a
    /// bounded texture, and the same Pin to Screen action as a display capture.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the worker already holds
    /// [`MAX_QUEUED_CAPTURE_FRAMES`] full-resolution frames, so a burst of
    /// forwarded captures is refused explicitly rather than queued without
    /// limit, and when the worker has stopped.
    pub fn accept_capture(
        &mut self,
        kind: CaptureKind,
        origin: CaptureOrigin,
        capture: Capture,
        policy: AfterCaptureSettings,
    ) -> CliResult<CardId> {
        let permit = self.capture_budget.try_acquire().ok_or_else(|| {
            CliError::Core(CoreError::Platform(format!(
                "the capture worker already holds the maximum of \
                 {MAX_QUEUED_CAPTURE_FRAMES} full-resolution forwarded captures"
            )))
        })?;
        let card = self.allocate();
        self.jobs
            .send(Job::Captured {
                kind,
                origin,
                card,
                capture,
                policy,
                _permit: permit,
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
        let policy = crate::settings::stored_settings()
            .map(|(persisted, _)| persisted)
            .unwrap_or_default();
        let posted = self.post(Job::Scrolling {
            axis,
            card,
            origin: CaptureOrigin::Direct,
            policy,
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

    /// Asks the active scrolling session to keep its partial image or abort it.
    ///
    /// Abort also cancels the acquisition token, because a session blocked in
    /// the compositor waiting for the next frame cannot observe a loop flag.
    pub fn cancel_scrolling(&self, action: CancelAction) -> bool {
        let accepted = self.scrolling_cancellation.cancel(action);
        if accepted
            && action == CancelAction::Abort
            && let Some(cancellation) = self
                .active_scrolling_acquisition
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
        {
            cancellation.cancel();
        }
        accepted
    }

    #[cfg(test)]
    pub fn seal_scrolling_output_for_test(&self) -> bool {
        self.scrolling_cancellation.seal_output()
    }

    /// Takes one finished piece of work, if there is one. Never blocks.
    pub fn poll(&self) -> Option<Outcome> {
        self.outcomes.try_recv().ok()
    }

    /// Injects an outcome as though a worker had produced it.
    ///
    /// Only an upload actually needs this: a real [`Outcome::UploadDone`]
    /// needs a network and, without the `cloud` feature, a backend that is
    /// not even compiled in, so it cannot be exercised hermetically by
    /// posting a real job. Every other outcome in these tests comes from
    /// posting a real [`Job`] and draining the real worker.
    #[cfg(test)]
    pub fn inject_outcome_for_test(&self, outcome: Outcome) {
        let _ = self.test_outcomes.send(outcome);
    }

    /// Cancels interactive acquisition, then stops and joins the worker.
    ///
    /// Called from `Drop`, but exposed so a host can shut down deterministically
    /// rather than at an unspecified point during teardown. Cancelling first
    /// closes an active Wayland ScreenCast session and dismisses its picker.
    pub fn stop(&mut self) {
        // The upload worker can still post work to the store-owning capture
        // worker, so it is drained first and the capture worker is stopped only
        // once no further jobs can arrive from it.
        self.upload_cancellation.cancel();
        let _ = self.uploads.send(UploadJob::Stop);
        if let Some(worker) = self.upload_worker.take() {
            let _ = worker.join();
        }
        // The upload worker can enqueue a final history update. Stop the
        // store-owning worker only after uploads are fully drained.
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
        let _ = self.history_queries.send(HistoryQuery::Stop);
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
    /// The card's own exportable capture — what a plain Copy, Save or Upload
    /// reads once no editor is open.
    ///
    /// Stays the untouched original through Cancel and a clean close. Per
    /// D14 that only changes on an explicit save, and Done is that explicit
    /// action ([`Worker::commit_card_output`] mirrors
    /// `App::refresh_card_thumbnail`'s Done-only call site exactly): a
    /// destructive redaction the card's thumbnail now shows committed must
    /// never be reachable through a card output that still hands back these
    /// pre-edit pixels.
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
            // Kept in step with `current_availability`: this executor runs on
            // the capture worker, and the upload worker exists precisely so a
            // slow provider cannot delay the next capture.
            AfterCaptureAction::UploadAndCopyLink => Err(
                "uploading during After Capture would hold the capture worker on a remote host; \
                 press Upload on the card instead"
                    .to_owned(),
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

    /// Replaces a card's own bytes with a committed editor revision.
    ///
    /// Unlike [`Self::store_rendered`], which stages a revision alongside the
    /// untouched capture for a drag that might still be cancelled, this is
    /// the one path that overwrites what a plain Copy, Save or Upload --
    /// posted once no editor is open -- actually reads. Called only for a
    /// committed (Done) edit, mirroring `App::refresh_card_thumbnail`'s
    /// Done-only call site; a cancelled or discarded edit must never reach
    /// this. The now-superseded staged revision is cleared with it, since
    /// keeping it would let a stale `get_revision` answer outlive the commit
    /// it was staged for.
    /// Replaces a card's own bytes with a committed editor revision.
    ///
    /// Unlike [`Self::store_rendered`], which stages a revision alongside the
    /// untouched capture for a drag that might still be cancelled, this is
    /// the one path that overwrites what a plain Copy, Save or Upload --
    /// posted once no editor is open -- actually reads. Called only for a
    /// committed (Done) edit, mirroring `App::refresh_card_thumbnail`'s
    /// Done-only call site; a cancelled or discarded edit must never reach
    /// this. The now-superseded staged revision is cleared with it, since
    /// keeping it would let a stale `get_revision` answer outlive the commit
    /// it was staged for.
    ///
    /// Backfills [`Cached::editor_source`] from the pre-commit bytes the
    /// first time a card without one (Smart Frame was off at capture)
    /// commits an edit. Once this returns, `bytes` is the flattened,
    /// annotated revision and can no longer serve as a reconstruction base;
    /// without this backfill a later reopen built from a freshly tracked
    /// document (see `Worker::commit_card_output`) would draw that
    /// document's own layer a second time over pixels that already show it.
    fn commit_rendered(&self, card: CardId, bytes: CaptureBytes) -> bool {
        let Ok(mut map) = self.inner.lock() else {
            return false;
        };
        let Some(cached) = map.get_mut(&card) else {
            return false;
        };
        if cached.editor_source.is_none() {
            cached.editor_source = Some(Arc::clone(&cached.bytes.full));
        }
        cached.bytes = bytes;
        cached.rendered = None;
        true
    }

    /// The full cached entry, including metadata needed to reopen the editor.
    fn cached(&self, card: CardId) -> Option<Cached> {
        self.inner.lock().ok()?.get(&card).cloned()
    }

    /// Clears a stale reconstruction base before a document is built directly
    /// from the card's current flattened pixels.
    ///
    /// Finding #3 (round 5): `Worker::open_cached_document`'s no-derived-
    /// document fallback builds a brand-new zero-history [`Document`] straight
    /// from [`Cached::bytes`]`.full` -- today's visible pixels -- not from
    /// whatever [`Cached::editor_source`] happens to still hold (a restore or
    /// an earlier session's edit can leave one in place). Leaving that stale
    /// value there would let [`Self::commit_rendered`]'s backfill-once guard
    /// skip updating it once this document is edited and committed, so a
    /// still-later reopen would draw the newly committed layers over an
    /// older, unrelated base -- potentially undoing a redaction that had
    /// already been flattened into `bytes.full`. Clearing it here means the
    /// very next commit backfills from exactly the pixels this document was
    /// actually built from.
    fn clear_editor_source(&self, card: CardId) {
        if let Ok(mut map) = self.inner.lock()
            && let Some(cached) = map.get_mut(&card)
        {
            cached.editor_source = None;
        }
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
    uploads: Sender<UploadJob>,
    scrolling_cancellation: AtomicCancellation,
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

/// Everything the worker needs to run one scrolling session.
struct ScrollingJob {
    axis: ScrollAxis,
    target: ScrollingTarget,
    acquisition_cancellation: scrozz_capture::CaptureCancellation,
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
        uploads: Sender<UploadJob>,
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
            uploads,
            scrolling_cancellation: AtomicCancellation::default(),
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

    /// Shares the keep/abort token the main thread signals scrolling with.
    ///
    /// Separate from [`Self::new`] so the constructor keeps one job — opening
    /// the store and restoring pins — and so the token is unmistakably the same
    /// object the [`Pipeline`] holds, not a second one that would silently
    /// never be signalled.
    fn with_scrolling_cancellation(mut self, cancellation: AtomicCancellation) -> Self {
        self.scrolling_cancellation = cancellation;
        self
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
                } => self.capture(kind, card, origin, &policy, None),
                Job::Scrolling {
                    axis,
                    card,
                    origin,
                    policy,
                    target,
                    acquisition_cancellation,
                } => self.capture(
                    CaptureKind::Scrolling,
                    card,
                    origin,
                    &policy,
                    Some(ScrollingJob {
                        axis,
                        target: *target,
                        acquisition_cancellation,
                    }),
                ),
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
                Job::CommitCardOutput {
                    card,
                    generation,
                    rendered,
                    data,
                } => self.commit_card_output(card, generation, &rendered, &data),
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
                    _permit,
                } => self.accept_captured(kind, origin, card, capture, &policy),
                Job::Copy { card, action } => self.copy(card, action),
                Job::CopyImage {
                    card,
                    generation,
                    rendered,
                    action,
                } => self.copy_image(card, generation, &rendered, action),
                Job::SaveImage {
                    card,
                    generation,
                    rendered,
                    action,
                } => self.save_image(card, generation, &rendered, action),
                Job::SaveImageTo {
                    card,
                    generation,
                    rendered,
                    path,
                    action,
                } => self.save_image_to(card, generation, &rendered, &path, action),
                Job::Save { card, action } => self.save(card, action),
                Job::SaveTo { card, path, action } => self.save_to(card, &path, action),
                Job::Upload { card, action } => self.upload(card, action),
                Job::UploadImage {
                    card,
                    generation,
                    rendered,
                    action,
                } => self.upload_image(card, generation, &rendered, action),
                Job::UploadRecording {
                    card,
                    capture,
                    path,
                    content_type,
                    file_name,
                    action,
                } => self.upload_recording(card, capture, &path, content_type, file_name, action),
                Job::RememberShare {
                    capture_id,
                    sharing,
                } => self.remember_share(&capture_id, sharing),
                Job::Release(card) => {
                    self.vault.forget(card);
                    self.derived_documents.remove(&card);
                    let _ = self.uploads.send(UploadJob::Release(card));
                }
                Job::ReleaseIfRetained { card, capture } => {
                    let released = self
                        .store
                        .as_ref()
                        .and_then(|store| store.record(&capture).ok().flatten())
                        .is_some_and(|record| matches!(record.image, ImageState::Present { .. }));
                    if released {
                        self.vault.forget(card);
                        self.derived_documents.remove(&card);
                    }
                    self.emit(Outcome::RetentionRelease { card, released });
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
        scrolling: Option<ScrollingJob>,
    ) {
        tracing::debug!(
            %card,
            capture = kind.label(),
            origin = origin.label(),
            "capture job started"
        );
        let mut lifecycle = CaptureLifecycle::new(Arc::clone(&self.selector));
        let result = self.take(kind, card, policy, &mut lifecycle, scrolling);
        match result {
            Ok(built) => {
                self.emit(Outcome::Ready(Box::new(built)));
            }
            Err(error) if error.is_cancellation() && kind != CaptureKind::Scrolling => {
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
        scrolling: Option<ScrollingJob>,
    ) -> CliResult<ReadyCapture> {
        // Through `platform`, not `scrozz_capture` directly, so the
        // SCROZZ_UNSTABLE_BACKENDS guard still applies to the GUI path.
        let backend = platform::capture_backend()?;
        if kind == CaptureKind::Scrolling {
            let job = scrolling.ok_or_else(|| {
                CliError::Core(CoreError::InvalidRequest(
                    "a scrolling pipeline job must name its axis and target".to_owned(),
                ))
            })?;
            let outcomes = self.outcomes.clone();
            let capture = crate::scrolling::scrolling_capture_target_with_cancellation(
                job.target,
                job.axis,
                &mut self.scrolling_cancellation,
                &job.acquisition_cancellation,
                move |progress| {
                    let _ = outcomes.send(Outcome::Progress { card, progress });
                },
            )?;
            lifecycle.finish();
            // An abort that arrived while the last frame was being stitched must
            // not produce a card, so it is checked after the session returns and
            // before anything durable is written.
            if !self.scrolling_cancellation.seal_output() {
                return Err(CliError::Core(CoreError::Cancelled));
            }
            return self.finish_capture(kind, card, capture, policy);
        }
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
            // Handled above, before any selector work: a scrolling session
            // resolves its own target on the main thread.
            CaptureKind::Scrolling => unreachable!("scrolling captures return earlier"),
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
                FrameEncoder::new().encode(&document.source().frame, ImageFormat::Png)?,
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
        let retained_in_history = capture_id.as_ref().is_some_and(|id| {
            self.store
                .as_ref()
                .and_then(|store| store.record(id).ok().flatten())
                .is_some_and(|record| matches!(record.image, ImageState::Present { .. }))
        });
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
                provenance: document.source().provenance,
                target: document.source().target.clone(),
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
        let exported = actions.steps.iter().any(|step| {
            matches!(
                step.outcome,
                crate::after_capture::ActionOutcome::Succeeded(ActionEffect::Saved(_))
            )
        });
        let retained_elsewhere = retained_in_history
            || actions.steps.iter().any(|step| {
                matches!(
                    step.outcome,
                    crate::after_capture::ActionOutcome::Succeeded(
                        ActionEffect::Saved(_) | ActionEffect::Uploaded(_)
                    )
                )
            });

        Ok(ReadyCapture {
            card: Card {
                id: card,
                media: scrozz_ui::card::CardMedia::Image,
                capture_id,
                kind,
                provenance: document.source().provenance,
                source_width: frame.width(),
                source_height: frame.height(),
                scale: frame.scale.get(),
                thumbnail,
                // History persistence is internal and does not count as an export.
                // A visible file is created only when the user presses Save; writing
                // one here made Save create a duplicate a few seconds later.
                written,
                taken_at: SystemTime::now(),
                upload_available: false,
                upload_unavailable_reason: None,
            },
            actions,
            retained_elsewhere,
            exported,
        })
    }

    fn prepare_after_capture_revision(
        document: &mut Document,
        apply_smart_frame: bool,
    ) -> CliResult<scrozz_core::Frame> {
        if !apply_smart_frame {
            return Ok(document.source().frame.clone());
        }
        let current = SkiaRenderer.render(document)?;
        let analysis = analyze_smart_frame(
            &current,
            document.source().provenance,
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
            CaptureKind::Fullscreen | CaptureKind::AllDisplays | CaptureKind::Scrolling => {
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
            Ok(capture) => {
                self.history_done(
                    HistoryOperation::Edit,
                    Some(capture),
                    None,
                    format!(
                        "{card} editor {generation} revision {revision} saved to capture history"
                    ),
                );
                // Ahead of `HistoryDone`, this is the ack an editor's close
                // actually gates on -- see `Outcome::EditorClosePersisted`.
                self.emit(Outcome::EditorClosePersisted {
                    card,
                    generation,
                    revision,
                });
            }
            Err(error) => {
                // The editor waiting on this must not release its
                // editor-only cache nor finalize closed on the strength of
                // this failure -- see `Outcome::EditorClosePersistFailed`.
                self.emit(Outcome::EditorClosePersistFailed {
                    card,
                    generation,
                    revision,
                    error,
                });
            }
        }
    }

    /// Files a Done-committed editor revision into the card's own bytes.
    ///
    /// Companion to [`Self::persist_document`], posted for the same commit:
    /// that call persists the editable scene to history, this one replaces
    /// what a plain Copy, Save or Upload actually reads once the editor is
    /// gone. Emits [`Outcome::CardOutputCommitted`] or
    /// [`Outcome::CardOutputCommitFailed`] either way (Finding #1, round 5):
    /// the caller's window must not finalize closed, nor let any output on
    /// this card read its own bytes, until it hears back -- posting this job
    /// is not the same as it having landed, and a stale card output would
    /// otherwise resurrect pixels a destructive redaction was meant to
    /// remove.
    ///
    /// Also refreshes [`Self::derived_documents`] with `data` in the same
    /// step as the bytes: `persist_document`'s history write can fail or
    /// lag independently, and [`Self::open`] must never reconstruct a
    /// document older than what was just committed here.
    fn commit_card_output(
        &mut self,
        card: CardId,
        generation: u64,
        rendered: &RevisionedFrame,
        data: &DocumentData,
    ) {
        let revision = rendered.revision();
        match CaptureBytes::from_rendered(generation, rendered) {
            Ok(bytes) => {
                if self.vault.commit_rendered(card, bytes) {
                    self.derived_documents.insert(card, data.clone());
                    // The window this belongs to is waiting on exactly this
                    // ack before it finalizes closed -- see
                    // `Outcome::CardOutputCommitted` (Finding #1, round 5).
                    self.emit(Outcome::CardOutputCommitted {
                        card,
                        generation,
                        revision,
                    });
                } else {
                    let error = CliError::Core(CoreError::Storage(format!(
                        "{card} had already left the vault"
                    )));
                    tracing::warn!(
                        %card,
                        "a committed edit could not be filed: the card had already left the vault"
                    );
                    self.emit(Outcome::CardOutputCommitFailed {
                        card,
                        generation,
                        revision,
                        error,
                    });
                }
            }
            Err(error) => {
                tracing::warn!(
                    %card,
                    %error,
                    "a committed edit could not be encoded for the card's own bytes"
                );
                self.emit(Outcome::CardOutputCommitFailed {
                    card,
                    generation,
                    revision,
                    error,
                });
            }
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
        // A committed edit refreshes this in the very same worker step that
        // replaces the card's own bytes (see `Worker::commit_card_output`),
        // so whenever an entry exists here it is always at least as fresh
        // as anything durable history might return -- including when the
        // history write posted alongside it (`Job::PersistDocument`) is
        // still pending or has failed outright. Trusting history over this
        // would risk resurrecting a stale, unredacted document even though
        // the card's own bytes and thumbnail already show the redaction
        // committed.
        let document = if self.derived_documents.contains_key(&card) {
            self.open_cached_document(card)
        } else {
            let durable = self.vault.cached(card).and_then(|cached| cached.capture_id);
            match durable {
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
            }
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
            None => {
                // No in-memory document survives for this card, so the
                // document about to be built is a fresh, zero-history one
                // over today's flattened pixels -- not over whatever
                // `editor_source` still remembers. See
                // `CaptureVault::clear_editor_source`.
                self.vault.clear_editor_source(card);
                self.capture_from_visible_cache(card, "open")
                    .map(Document::new)
            }
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
                let provenance = document.source().provenance;
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

    fn copy_image(
        &mut self,
        card: CardId,
        generation: u64,
        rendered: &RevisionedFrame,
        action: u64,
    ) {
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
        self.answer(
            card,
            Some((generation, rendered.revision())),
            action,
            result,
        );
    }

    fn save_image(
        &mut self,
        card: CardId,
        generation: u64,
        rendered: &RevisionedFrame,
        action: u64,
    ) {
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
        self.answer(
            card,
            Some((generation, rendered.revision())),
            action,
            result,
        );
    }

    fn save_image_to(
        &mut self,
        card: CardId,
        generation: u64,
        rendered: &RevisionedFrame,
        path: &std::path::Path,
        action: u64,
    ) {
        tracing::debug!(
            %card,
            revision = rendered.revision(),
            destination = %path.display(),
            "saving a rendered document revision to a chosen destination"
        );
        let result = FrameEncoder::new()
            .encode(rendered.frame(), ImageFormat::Png)
            .map_err(CliError::from)
            .and_then(|bytes| {
                let path = crate::output::export_to_path(&bytes, path)?;
                Ok(format!("saved the annotated image to {}", path.display()))
            });
        self.answer(
            card,
            Some((generation, rendered.revision())),
            action,
            result,
        );
    }

    fn copy(&mut self, card: CardId, action: u64) {
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
        self.answer(card, None, action, result);
    }

    fn save(&mut self, card: CardId, action: u64) {
        let result = self.cached(card, "save").and_then(|cached| {
            let path = crate::output::export_default(&cached.full)?;
            Ok(format!("saved to {}", path.display()))
        });
        self.answer(card, None, action, result);
    }

    fn save_to(&mut self, card: CardId, path: &std::path::Path, action: u64) {
        let result = self.cached(card, "save").and_then(|cached| {
            let path = crate::output::export_to_path(&cached.full, path)?;
            Ok(format!("saved to {}", path.display()))
        });
        self.answer(card, None, action, result);
    }

    /// Hands the card's *current* bytes to the upload worker.
    ///
    /// Nothing about the remote host is touched here: this worker owns the
    /// store connection and the shutter path, so all it does is copy an `Arc`
    /// into a channel. The revision travelling with the bytes is the vault's
    /// own, so a share can never be attributed to a revision the user did not
    /// approve — see [`CaptureBytes`].
    fn upload(&mut self, card: CardId, action: u64) {
        let result = self.cached_entry(card, "upload").and_then(|cached| {
            let artifact = crate::cloud::FinalizedArtifact::screenshot_png(
                cached.bytes.full.as_ref().clone(),
                format!("Screenshot-{}.png", card.0),
            )?;
            self.queue_upload(
                card,
                cached.capture_id.clone(),
                ShareVersion::of(&cached.bytes),
                artifact,
                action,
            )
        });
        self.answer_upload(card, action, result);
    }

    /// Uploads the exact revision an open editor produced, never the original.
    ///
    /// The revision travels with the bytes, so a share can only ever be
    /// attributed to the pixels the user approved — the same rule Copy, Save
    /// and drag already follow for a destructively redacted document.
    fn upload_image(
        &mut self,
        card: CardId,
        generation: u64,
        rendered: &RevisionedFrame,
        action: u64,
    ) {
        let capture_id = self.vault.cached(card).and_then(|cached| cached.capture_id);
        let result = FrameEncoder::new()
            .encode(rendered.frame(), ImageFormat::Png)
            .map_err(CliError::from)
            .and_then(|png| {
                crate::cloud::FinalizedArtifact::screenshot_png(
                    png,
                    format!("Screenshot-{}.png", card.0),
                )
            })
            .and_then(|artifact| {
                self.queue_upload(
                    card,
                    capture_id,
                    ShareVersion {
                        generation: Some(generation),
                        revision: rendered.revision(),
                    },
                    artifact,
                    action,
                )
            });
        self.answer_upload(card, action, result);
    }

    /// Uploads a durable recording a card is showing.
    ///
    /// The file is read here rather than on the main thread: a screen recording
    /// is tens of megabytes, and the frame that reads it is the frame that drops.
    fn upload_recording(
        &mut self,
        card: CardId,
        capture: Option<CaptureId>,
        path: &std::path::Path,
        content_type: String,
        file_name: String,
        action: u64,
    ) {
        let result = std::fs::read(path)
            .map_err(|error| {
                CliError::Core(CoreError::Platform(format!(
                    "could not read {} to upload it: {error}",
                    path.display()
                )))
            })
            .and_then(|bytes| {
                crate::cloud::FinalizedArtifact::recording(bytes, content_type, file_name)
            })
            .and_then(|artifact| {
                // A finished recording's durable file is immutable for the
                // lifetime of its card, so it is always revision zero.
                self.queue_upload(card, capture, ShareVersion::original(), artifact, action)
            });
        self.answer_upload(card, action, result);
    }

    fn queue_upload(
        &self,
        card: CardId,
        capture_id: Option<CaptureId>,
        version: ShareVersion,
        artifact: crate::cloud::FinalizedArtifact,
        action: u64,
    ) -> CliResult<()> {
        self.uploads
            .send(UploadJob::Share {
                card,
                capture_id,
                version,
                artifact,
                action,
            })
            .map_err(|_| {
                CliError::Core(CoreError::Platform("the upload worker has gone".to_owned()))
            })
    }

    /// Reports whether the upload reached its worker.
    ///
    /// A refusal here is [`Outcome::UploadRefused`] rather than
    /// [`Outcome::OutputRefused`] so the card shows the reason: the user
    /// pressed Upload and is watching that card for an answer.
    fn answer_upload(&mut self, card: CardId, action: u64, result: CliResult<()>) {
        let outcome = match result {
            Ok(()) => Outcome::Started {
                card,
                detail: "upload queued".to_owned(),
            },
            Err(error) => Outcome::UploadRefused {
                card,
                error,
                action,
            },
        };
        // Round 9, Finding #2: this local refusal (encoding failure, a
        // missing cache entry, a read failure, or the upload worker's queue
        // having gone) previously sent straight to `self.outcomes` rather
        // than through the wake-aware `Self::emit` every other outcome in
        // this worker uses. In a reactive event loop that is woken only by
        // `SurfaceWaker`, that left the answer sitting in the channel with
        // nothing to prompt draining it.
        self.emit(outcome);
    }

    /// Attaches secret-free remote metadata to the capture's history row.
    ///
    /// Runs here because this worker owns the store connection. A failure is a
    /// warning, not a refusal: the share itself already succeeded, and losing
    /// its bookkeeping must not read to the user as a failed upload.
    fn remember_share(&mut self, capture_id: &CaptureId, sharing: RememberedShare) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        let result = sharing
            .into_history()
            .and_then(|sharing| store.set_share_metadata(capture_id, Some(sharing)));
        if let Err(error) = result {
            tracing::warn!(capture = %capture_id.0, "could not attach share metadata to history: {error}");
        }
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
                upload_available: false,
                upload_unavailable_reason: None,
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
            Arc::new(FrameEncoder::new().encode(&document.source().frame, ImageFormat::Png)?);
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

    fn answer(
        &self,
        card: CardId,
        version: Option<(u64, u64)>,
        action: u64,
        result: CliResult<String>,
    ) {
        let message = match result {
            Ok(detail) => Outcome::Done {
                card,
                detail,
                version,
                action,
            },
            Err(error) => Outcome::OutputRefused {
                card,
                error,
                action,
            },
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
                let preview = history_thumbnail(&document, document.source().frame.scale)?;
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

/// The card kind a capture's provenance implies.
///
/// The pin's chrome policy is chosen from the card kind, and D9 forbids
/// synthetic chrome on a window capture, so this mapping is part of the
/// contract rather than a presentation detail. It reads the *provenance* rather
/// than the requested target because an interactive request does not know what
/// the user will pick.
pub(crate) const fn capture_kind(provenance: Provenance) -> CaptureKind {
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

/// The exact revision a share was made from.
///
/// Taken straight from the vault's [`CaptureBytes`] or the editor's
/// [`RevisionedFrame`] rather than counted separately here: one authority for
/// "which pixels is this" means a cached link can never be handed out for a
/// revision the user has since replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShareVersion {
    generation: Option<u64>,
    revision: u64,
}

impl ShareVersion {
    const fn original() -> Self {
        Self {
            generation: None,
            revision: 0,
        }
    }

    const fn of(bytes: &CaptureBytes) -> Self {
        Self {
            generation: bytes.generation(),
            revision: bytes.revision(),
        }
    }
}

enum UploadJob {
    Share {
        card: CardId,
        capture_id: Option<CaptureId>,
        version: ShareVersion,
        artifact: crate::cloud::FinalizedArtifact,
        /// See [`Outcome::UploadDone::action`].
        action: u64,
    },
    Release(CardId),
    Stop,
}

struct UploadWorker {
    outcomes: Sender<Outcome>,
    /// Wakes the reactive event loop after every outcome send, mirroring
    /// [`HistoryReader`]'s own wake-on-send (round 8, Finding #5).
    ///
    /// Without this, a success or refusal delivered here sits in the
    /// channel until *something else* happens to wake the window (a mouse
    /// move, a repaint the OS scheduled for an unrelated reason, ...); on a
    /// genuinely idle window the reactive loop can be parked indefinitely
    /// and never drains it at all, leaving an upload silently "stuck"
    /// forever from the user's perspective even though the worker finished
    /// long ago.
    waker: Option<SurfaceWaker>,
    cancellation: crate::cloud::ShareCancellation,
    history: Sender<Job>,
    links: HashMap<CardId, CachedShare>,
}

struct CachedShare {
    shared: crate::cloud::Shared,
    expires_at: Option<SystemTime>,
    /// Editor lifetime and document revision the remote object was made from.
    ///
    /// Reusing a link is only safe while it still points at the pixels on the
    /// card. A destructive redaction bumps the revision, so comparing this
    /// against the incoming artifact is what stops a second Upload from handing
    /// out a link to the *previous* revision.
    version: ShareVersion,
}

impl CachedShare {
    fn new(shared: crate::cloud::Shared, version: ShareVersion) -> Self {
        let expires_at = shared.expires_at;
        Self {
            shared,
            expires_at,
            version,
        }
    }

    fn is_expired(&self) -> bool {
        self.expires_at
            .is_some_and(|expires_at| SystemTime::now() >= expires_at)
    }
}

impl UploadWorker {
    fn new(
        outcomes: Sender<Outcome>,
        waker: Option<SurfaceWaker>,
        cancellation: crate::cloud::ShareCancellation,
        history: Sender<Job>,
    ) -> Self {
        Self {
            outcomes,
            waker,
            cancellation,
            history,
            links: HashMap::new(),
        }
    }

    /// Sends `outcome` and, only once it actually landed, wakes the
    /// reactive event loop so it drains promptly rather than waiting on
    /// some unrelated repaint (round 8, Finding #5).
    fn send_outcome(&self, outcome: Outcome) {
        if self.outcomes.send(outcome).is_ok()
            && let Some(waker) = &self.waker
        {
            waker();
        }
    }

    fn run(mut self, jobs: &Receiver<UploadJob>) {
        while let Ok(job) = jobs.recv() {
            match job {
                UploadJob::Share {
                    card,
                    capture_id,
                    version,
                    artifact,
                    action,
                } => {
                    if self.cancellation.is_cancelled() {
                        break;
                    }
                    self.upload(card, capture_id, version, &artifact, action);
                }
                UploadJob::Release(card) => {
                    self.links.remove(&card);
                }
                UploadJob::Stop => break,
            }
        }
        tracing::debug!("upload worker stopped");
    }

    fn upload(
        &mut self,
        card: CardId,
        capture_id: Option<CaptureId>,
        version: ShareVersion,
        artifact: &crate::cloud::FinalizedArtifact,
        action: u64,
    ) {
        if self
            .links
            .get(&card)
            .is_some_and(|shared| shared.is_expired() || shared.version != version)
        {
            self.links.remove(&card);
        }
        if !self.links.contains_key(&card) {
            match crate::cloud::share_artifact(artifact, card.0, &self.cancellation) {
                Ok(shared) => {
                    if let Some(capture_id) = capture_id {
                        let _ = self.history.send(Job::RememberShare {
                            capture_id,
                            sharing: RememberedShare::from_shared(&shared),
                        });
                    }
                    self.links.insert(card, CachedShare::new(shared, version));
                }
                Err(error) => {
                    self.send_outcome(Outcome::UploadRefused {
                        card,
                        error,
                        action,
                    });
                    return;
                }
            }
        }
        let Some(shared) = self.links.get(&card) else {
            self.send_outcome(Outcome::UploadRefused {
                card,
                error: CliError::Core(CoreError::Platform(
                    "the upload completed without a retained share link".to_owned(),
                )),
                action,
            });
            return;
        };
        let outcome = match scrozz_export::SystemClipboard::new().write_text(&shared.shared.url) {
            Ok(()) => Outcome::UploadDone {
                card,
                detail: "uploaded and copied the private share link".to_owned(),
                version: shared
                    .version
                    .generation
                    .map(|g| (g, shared.version.revision)),
                action,
            },
            Err(error) => Outcome::UploadRefused {
                card,
                error: CliError::Core(CoreError::Platform(format!(
                    "the upload succeeded, but its link could not be copied: {error}. \
                     Press Upload again to retry the clipboard; Scrozz reuses the object while \
                     its signed URL remains valid"
                ))),
                action,
            },
        };
        self.send_outcome(outcome);
    }
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
        let (worker, inbox, _uploads) = worker_holding_with_uploads(card, cached);
        (worker, inbox)
    }

    /// The same worker, with the upload channel kept so a test can read it.
    fn worker_holding_with_uploads(
        card: CardId,
        cached: Cached,
    ) -> (Worker, Receiver<Outcome>, Receiver<UploadJob>) {
        let (outcomes, inbox) = std::sync::mpsc::channel();
        let (uploads, upload_inbox) = std::sync::mpsc::channel();
        let vault = CaptureVault::new();
        vault.store(card, cached);
        let worker = Worker {
            outcomes,
            uploads,
            scrolling_cancellation: AtomicCancellation::default(),
            waker: None,
            selector: Arc::new(RefusingSelector),
            store: None,
            vault,
            pin_generations: HashMap::new(),
            retention_policy: RetentionPolicy::default(),
            derived_documents: HashMap::new(),
            analysis_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        (worker, inbox, upload_inbox)
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
            (document.source().frame.scale.get() - 2.0).abs() < f64::EPSILON,
            "the capture's own scale should survive the cache, not decode's identity"
        );
        assert_eq!(
            document.source().frame.color_space,
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
        let logical =
            f64::from(document.source().frame.width()) / document.source().frame.scale.get();
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
        assert_eq!(document.source().frame.color_space, ColorSpace::Unknown);
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
        assert_eq!(document.source().provenance, Provenance::Window);
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
        assert_eq!(document.source().provenance, Provenance::Region);
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
        assert_eq!(document.source().frame.width(), 2);
        assert_eq!(document.source().frame.height(), 2);
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

    fn large_capture(marker: u8) -> Capture {
        let edge = 1_024;
        Capture {
            frame: Frame {
                data: vec![marker; edge * edge * 4],
                size: PhysicalSize::new(edge as f64, edge as f64),
                stride: edge * 4,
                format: PixelFormat::Rgba8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::IDENTITY,
            },
            provenance: Provenance::Window,
            target: CaptureTarget::Window(WindowId(format!("large-{marker}"))),
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
                uploads: channel().0,
                scrolling_cancellation: AtomicCancellation::default(),
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
        let source = sample_document(96, 60, 1, 0).source().clone();
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
        assert_eq!(stored.source().frame.data, source_pixels);
    }

    #[test]
    fn disabled_after_capture_smart_frame_is_a_byte_stable_noop() {
        let mut document = sample_document(32, 20, 1, 0);
        let source = document.source().frame.clone();

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
        let source = sample_document(96, 60, 1, 0).source().clone();
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
        assert_eq!(document.source().frame.width(), 96);
        assert!(
            document
                .beautification()
                .is_some_and(|beautification| beautification.auto_balance)
        );
    }

    #[test]
    fn asynchronous_smart_frame_analysis_is_revision_bound_and_cached() {
        let card = CardId(7);
        let capture = sample_document(80, 50, 1, 0).source().clone();
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
        let capture = sample_document(80, 50, 1, 0).source().clone();
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
            .remember(sample_document(8, 8, 1, 0).source())
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
            .remember(sample_document(8, 8, 2, 0).source())
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
    fn full_resolution_capture_backpressure_is_strict_and_moves_pixels() {
        let (jobs, job_rx) = channel();
        let (history_queries, _history_rx) = channel();
        let (uploads, _upload_rx) = channel();
        let (outcome_tx, outcomes) = channel();
        let mut pipeline = Pipeline {
            jobs,
            capture_budget: CaptureBudget::new(),
            worker_done: channel().1,
            scrolling_cancellation: AtomicCancellation::default(),
            active_scrolling_acquisition: Mutex::new(None),
            pending_pin_updates: Arc::new(PendingPinUpdates::default()),
            history_queries,
            uploads,
            outcomes,
            #[cfg(test)]
            test_outcomes: outcome_tx,
            worker: None,
            history_worker: None,
            upload_worker: None,
            upload_cancellation: crate::cloud::ShareCancellation::default(),
            next_card: 1,
            vault: CaptureVault::new(),
        };
        let policy = AfterCaptureSettings::default();

        let first = large_capture(1);
        let first_pixels = first.frame.data.as_ptr();
        pipeline
            .accept_capture(
                CaptureKind::Window,
                CaptureOrigin::Direct,
                first,
                policy.clone(),
            )
            .expect("first large frame");
        pipeline
            .accept_capture(
                CaptureKind::Region,
                CaptureOrigin::Direct,
                large_capture(2),
                policy.clone(),
            )
            .expect("second large frame");
        let error = pipeline
            .accept_capture(
                CaptureKind::Window,
                CaptureOrigin::Direct,
                large_capture(3),
                policy,
            )
            .expect_err("a burst beyond the strict frame budget must backpressure");
        assert!(
            error.to_string().contains("maximum"),
            "backpressure should be explicit: {error}"
        );
        assert_eq!(
            pipeline.capture_budget.in_flight.load(Ordering::Acquire),
            MAX_QUEUED_CAPTURE_FRAMES
        );

        let Job::Captured { capture, .. } = job_rx.try_recv().expect("first queued capture") else {
            panic!("capture admission queued the wrong job");
        };
        assert_eq!(
            capture.frame.data.as_ptr(),
            first_pixels,
            "the full-resolution pixel allocation must move, not clone"
        );
    }

    #[test]
    fn a_drained_capture_permit_readmits_the_next_forwarded_frame() {
        // The budget must be a live count, not a high-water mark: once the
        // worker has taken a frame the next forwarded capture has to be
        // admitted, or a single burst would wedge forwarding for the session.
        let (jobs, job_rx) = channel();
        let (history_queries, _history_rx) = channel();
        let (uploads, _upload_rx) = channel();
        let (outcome_tx, outcomes) = channel();
        let mut pipeline = Pipeline {
            jobs,
            capture_budget: CaptureBudget::new(),
            worker_done: channel().1,
            scrolling_cancellation: AtomicCancellation::default(),
            active_scrolling_acquisition: Mutex::new(None),
            pending_pin_updates: Arc::new(PendingPinUpdates::default()),
            history_queries,
            uploads,
            outcomes,
            #[cfg(test)]
            test_outcomes: outcome_tx,
            worker: None,
            history_worker: None,
            upload_worker: None,
            upload_cancellation: crate::cloud::ShareCancellation::default(),
            next_card: 1,
            vault: CaptureVault::new(),
        };
        let policy = AfterCaptureSettings::default();

        for marker in 1..=MAX_QUEUED_CAPTURE_FRAMES {
            pipeline
                .accept_capture(
                    CaptureKind::Window,
                    CaptureOrigin::Direct,
                    large_capture(u8::try_from(marker).expect("small marker")),
                    policy.clone(),
                )
                .expect("frames within the budget");
        }
        assert!(
            pipeline
                .accept_capture(
                    CaptureKind::Window,
                    CaptureOrigin::Direct,
                    large_capture(9),
                    policy.clone()
                )
                .is_err()
        );

        drop(job_rx.try_recv().expect("one queued capture"));
        pipeline
            .accept_capture(
                CaptureKind::Window,
                CaptureOrigin::Direct,
                large_capture(10),
                policy,
            )
            .expect("a drained permit readmits one frame");
        assert_eq!(
            pipeline.capture_budget.in_flight.load(Ordering::Acquire),
            MAX_QUEUED_CAPTURE_FRAMES
        );
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
        assert!(pipeline.post(Job::Copy {
            card: CardId(404),
            action: 1,
        }));
        let _ = wait_for(&pipeline);
        assert!(wakes.load(std::sync::atomic::Ordering::Relaxed) > 0);
    }

    /// Round 9, Finding #2: `Worker::answer_upload`'s local refusal path (a
    /// missing cache entry here, but the same fix also covers an encoding
    /// failure, a read failure, and the upload worker's queue having gone)
    /// previously sent straight to the outcome channel, bypassing
    /// `Worker::emit`'s wake-aware send every other outcome in this worker
    /// uses. A reactive event loop woken only by `SurfaceWaker` could then
    /// leave the refusal sitting undrained in the channel.
    #[test]
    fn a_locally_refused_upload_wakes_the_window_event_loop() {
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&wakes);
        let waker: SurfaceWaker = Arc::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        let pipeline =
            Pipeline::start_with_history_and_waker(Arc::new(RefusingSelector), false, Some(waker))
                .expect("worker");
        // Never captured, so `cached_entry` refuses before the upload ever
        // reaches the upload worker -- exactly the local-refusal branch of
        // `answer_upload` this finding covers.
        assert!(pipeline.post(Job::Upload {
            card: CardId(9001),
            action: 1,
        }));
        match wait_for(&pipeline) {
            Some(Outcome::UploadRefused { card, action, .. }) => {
                assert_eq!(card, CardId(9001));
                assert_eq!(action, 1);
            }
            other => panic!("expected a local upload refusal, got {other:?}"),
        }
        assert!(
            wakes.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "a locally refused upload must wake the event loop"
        );
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
    fn committing_a_card_output_replaces_its_exportable_bytes_with_the_edited_revision() {
        let (_dir, mut worker, _receiver) = worker_with_store("worker-commit-output");
        let original = richly_annotated_document(1);
        let card = CardId(21);
        worker
            .vault
            .store_test_capture(card, original.source())
            .expect("seed the card's own capture");
        let original_bytes = worker.vault.get(card).expect("seeded capture").full;

        let edited = richly_annotated_document(9);
        let rendered = RevisionedFrame::from_document(&edited, 3).expect("render edited revision");
        worker.commit_card_output(card, 1, &rendered, &edited.data());

        let committed = worker.vault.get(card).expect("card remains in the vault");
        assert_eq!(
            committed.revision(),
            3,
            "the committed revision must be recorded"
        );
        assert_eq!(
            committed.generation(),
            Some(1),
            "the committed bytes must be attributed to the editor that produced them"
        );
        assert_ne!(
            committed.full, original_bytes,
            "a plain Copy, Save or Upload posted after Done must not fall back to the pre-edit \
             capture -- exactly the pixels a destructive redaction was meant to remove"
        );
        let decoded = scrozz_export::decode(&committed.full).expect("decode committed bytes");
        let expected = SkiaRenderer::new()
            .render(&edited)
            .expect("reference render");
        assert_eq!(
            decoded.data, expected.data,
            "the committed bytes must be the exact edited revision, not a different render"
        );
        assert!(
            worker.vault.get_revision(card, 1, 3).is_none(),
            "the staged revision is superseded by the commit and must not linger"
        );
    }

    #[test]
    fn a_committed_card_output_for_a_vault_entry_that_has_left_is_a_harmless_no_op() {
        let (_dir, mut worker, _receiver) = worker_with_store("worker-commit-output-gone");
        let card = CardId(22);
        let edited = richly_annotated_document(2);
        let rendered = RevisionedFrame::from_document(&edited, 1).expect("render edited revision");

        // The card already left the vault (its editor-only release beat the
        // commit, or it was never captured through this worker) -- filing the
        // commit must not panic and must leave nothing behind.
        worker.commit_card_output(card, 1, &rendered, &edited.data());

        assert!(worker.vault.get(card).is_none());
    }

    #[test]
    fn a_first_commit_backfills_editor_source_from_the_pre_edit_pixels() {
        // Finding #3 (round 4), the corruption this backfill exists to
        // prevent: once a commit overwrites `bytes` with the flattened,
        // annotated render, that render can no longer serve as the base a
        // reopen rebuilds a document on top of -- doing so would draw the
        // very same annotations a second time. `editor_source` is the base
        // a reopen actually uses (see `capture_from_cache`); a card whose
        // Smart Frame was off at capture never had one populated at all.
        let (_dir, mut worker, _receiver) = worker_with_store("worker-commit-backfill-source");
        let card = CardId(23);
        let original = richly_annotated_document(1);
        worker
            .vault
            .store_test_capture(card, original.source())
            .expect("seed the card's own capture");
        let pre_commit_bytes = worker.vault.get(card).expect("seeded capture").full;
        assert!(
            worker
                .vault
                .cached(card)
                .expect("seeded")
                .editor_source
                .is_none(),
            "sanity: a card with Smart Frame off at capture starts without editor_source"
        );

        let edited = richly_annotated_document(9);
        let rendered = RevisionedFrame::from_document(&edited, 1).expect("render edited revision");
        worker.commit_card_output(card, 1, &rendered, &edited.data());

        let after = worker
            .vault
            .cached(card)
            .expect("card remains in the vault");
        assert_eq!(
            after.editor_source,
            Some(pre_commit_bytes),
            "editor_source must be backfilled with the pre-commit pixels, not left empty or \
             overwritten with the committed (already annotated) bytes"
        );
        assert_ne!(
            after.bytes.full,
            after.editor_source.expect("checked above"),
            "the card's own bytes must still be the newly committed render, not the backfilled \
             source"
        );
    }

    #[test]
    fn opening_a_committed_card_never_resurrects_a_stale_durable_document() {
        // Finding #3 (round 4): `Job::PersistDocument` (durable history) and
        // `Job::CommitCardOutput` (the vault's own bytes) are two
        // independently-failable jobs posted from the same Done. If
        // history's write lags or silently fails, `load_document` would
        // otherwise keep handing back whatever it last held -- an older,
        // possibly unredacted document -- even though the card's own bytes
        // already show the edit committed. `derived_documents` is refreshed
        // in the very same step that replaces those bytes, so `open` must
        // prefer it whenever both an entry and durable history exist.
        let (_dir, mut worker, receiver) = worker_with_store("worker-open-prefers-derived");
        let stale = richly_annotated_document(1);
        let capture_id = worker
            .remember_document(&stale)
            .expect("seed the stale durable history entry");

        let card = CardId(30);
        let seed_capture = stale.source();
        let full = FrameEncoder::new()
            .encode(&seed_capture.frame, ImageFormat::Png)
            .map(Arc::new)
            .expect("encode seed capture");
        worker.vault.store(
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
                provenance: seed_capture.provenance,
                target: seed_capture.target.clone(),
                scale: seed_capture.frame.scale,
                color_space: seed_capture.frame.color_space,
                capture_id: Some(capture_id),
            },
        );

        let edited = richly_annotated_document(9);
        let rendered = RevisionedFrame::from_document(&edited, 2).expect("render edited revision");
        worker.commit_card_output(card, 1, &rendered, &edited.data());
        assert!(
            matches!(received(&receiver), Outcome::CardOutputCommitted { .. }),
            "the commit itself must be acknowledged before the reopen below (round 5, Finding #1)"
        );

        worker.open(card);
        let Outcome::Opened { document, .. } = received(&receiver) else {
            panic!("opening a committed card should still produce a document");
        };
        assert_eq!(
            document.data(),
            edited.data(),
            "a reopen must never be older than the bytes it just committed, even when \
             durable history still holds a stale pre-edit document"
        );
    }

    #[test]
    fn a_flattened_visible_fallback_reopen_refreshes_the_reconstruction_source_before_it_next_commits()
     {
        // Finding #3 (round 5): a card can carry a stale `editor_source` --
        // left behind by an earlier restore or a since-forgotten edit --
        // while `derived_documents` no longer has an in-memory document and
        // there is no durable capture to load from. The next open then falls
        // back to a brand-new zero-history document built straight from the
        // card's *current* flattened pixels (`bytes.full`), which need not
        // be anything like that stale `editor_source`. Before this fix, the
        // stale value survived the open untouched, and the very next commit's
        // backfill-once guard would then skip updating it -- so a still
        // later reopen would draw the new commit's layers over years-old,
        // unrelated (possibly unredacted) pixels instead of the ones this
        // document was actually built from.
        let (_dir, mut worker, receiver) = worker_with_store("worker-fallback-source-refresh");
        let card = CardId(31);
        let stale_original = richly_annotated_document(1).source().clone();
        let flattened_redacted = richly_annotated_document(2).source().clone();
        let stale_original_bytes = FrameEncoder::new()
            .encode(&stale_original.frame, ImageFormat::Png)
            .map(Arc::new)
            .expect("encode stale original");
        let flattened_bytes = FrameEncoder::new()
            .encode(&flattened_redacted.frame, ImageFormat::Png)
            .map(Arc::new)
            .expect("encode flattened pixels");
        worker.vault.store(
            card,
            Cached {
                bytes: CaptureBytes {
                    generation: None,
                    revision: 0,
                    full: Arc::clone(&flattened_bytes),
                    preview: None,
                },
                editor_source: Some(Arc::clone(&stale_original_bytes)),
                rendered: None,
                provenance: flattened_redacted.provenance,
                target: flattened_redacted.target.clone(),
                scale: flattened_redacted.frame.scale,
                color_space: flattened_redacted.frame.color_space,
                capture_id: None,
            },
        );

        worker.open(card);
        assert!(
            matches!(received(&receiver), Outcome::Opened { .. }),
            "the flattened-visible fallback must still open successfully"
        );
        assert!(
            worker
                .vault
                .cached(card)
                .expect("card remains cached")
                .editor_source
                .is_none(),
            "opening the fallback document must clear the stale reconstruction source, not \
             leave it pointing at pixels the new document was never built from"
        );

        let edited = richly_annotated_document(9);
        let rendered = RevisionedFrame::from_document(&edited, 1).expect("render edited revision");
        worker.commit_card_output(card, 1, &rendered, &edited.data());

        let after_commit = worker
            .vault
            .cached(card)
            .expect("card remains cached after commit");
        assert_eq!(
            after_commit.editor_source,
            Some(flattened_bytes),
            "the backfill after this commit must use the pixels the fallback document was \
             actually built from"
        );
        assert_ne!(
            after_commit.editor_source,
            Some(stale_original_bytes),
            "the backfilled source must never regress to the older, unrelated pixels the stale \
             editor_source pointed at"
        );
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
        let raw = Thumbnail::from_frame(&document.source().frame, PIN_TEXTURE_MAX_EDGE)
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
    fn cached_share_expiry_is_enforced_before_clipboard_retry() {
        let expiring = CachedShare {
            shared: crate::cloud::Shared {
                url: "https://example.test/private".to_owned(),
                key: "capture.png".to_owned(),
                provider: "aws",
                expires_seconds: Some(1),
                expires_at: Some(std::time::UNIX_EPOCH),
                lifecycle_rule: None,
                encrypted: false,
                tags: Vec::new(),
                media_kind: crate::cloud::ArtifactKind::Screenshot,
            },
            expires_at: Some(std::time::UNIX_EPOCH),
            version: ShareVersion::original(),
        };
        assert!(expiring.is_expired());

        let public = CachedShare {
            expires_at: None,
            ..expiring
        };
        assert!(!public.is_expired());
    }

    #[test]
    fn upload_done_carries_the_exact_revision_the_cached_link_was_made_from() {
        // Round 5, Finding #2: `Outcome::UploadDone` must carry the editor
        // generation and document revision the uploaded object actually
        // represents, the same way `Outcome::Done` does for Copy/Save --
        // otherwise the main thread cannot tell a completion for a
        // since-superseded revision apart from one for the card's current
        // committed content, and could mark a stale revision retained.
        // A cache hit (no network call) is the deterministic way to exercise
        // this without a live `cloud` backend.
        let (outcomes, outcome_rx) = std::sync::mpsc::channel();
        let (history, _history_rx) = std::sync::mpsc::channel();
        let version = ShareVersion {
            generation: Some(3),
            revision: 11,
        };
        let mut worker = UploadWorker::new(
            outcomes,
            None,
            crate::cloud::ShareCancellation::default(),
            history,
        );
        worker.links.insert(
            CardId(50),
            CachedShare {
                shared: crate::cloud::Shared {
                    url: "https://example.test/cached".to_owned(),
                    key: "capture.png".to_owned(),
                    provider: "aws",
                    expires_seconds: None,
                    expires_at: None,
                    lifecycle_rule: None,
                    encrypted: false,
                    tags: Vec::new(),
                    media_kind: crate::cloud::ArtifactKind::Screenshot,
                },
                expires_at: None,
                version,
            },
        );

        let artifact = crate::cloud::FinalizedArtifact::screenshot_png(
            one_pixel_png(),
            "capture.png".to_owned(),
        )
        .expect("a nonempty PNG is a valid artifact");
        worker.upload(CardId(50), None, version, &artifact, 21);

        match outcome_rx.try_recv() {
            Ok(Outcome::UploadDone {
                card,
                version: got,
                action,
                ..
            }) => {
                assert_eq!(card, CardId(50));
                assert_eq!(
                    got,
                    Some((3, 11)),
                    "UploadDone must report the exact (generation, revision) the \
                     reused cached link was made from, not an absent or stale pair"
                );
                assert_eq!(
                    action, 21,
                    "UploadDone must report the exact action id it answers for \
                     (round 7, Finding #2)"
                );
            }
            other => panic!("expected an UploadDone from the cached link, got {other:?}"),
        }
    }

    #[test]
    fn a_successful_upload_wakes_the_window_event_loop() {
        // Round 8, Finding #5: unlike `HistoryReader`, `UploadWorker` sent its
        // outcomes without a `SurfaceWaker`, so a success or refusal it
        // delivered could sit undrained until something unrelated woke the
        // window. A cache hit exercises the success path deterministically,
        // with no live `cloud` backend and no network call.
        let (outcomes, outcome_rx) = std::sync::mpsc::channel();
        let (history, _history_rx) = std::sync::mpsc::channel();
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&wakes);
        let waker: SurfaceWaker = Arc::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        let version = ShareVersion {
            generation: Some(1),
            revision: 1,
        };
        let mut worker = UploadWorker::new(
            outcomes,
            Some(waker),
            crate::cloud::ShareCancellation::default(),
            history,
        );
        worker.links.insert(
            CardId(60),
            CachedShare {
                shared: crate::cloud::Shared {
                    url: "https://example.test/cached-wake".to_owned(),
                    key: "capture.png".to_owned(),
                    provider: "aws",
                    expires_seconds: None,
                    expires_at: None,
                    lifecycle_rule: None,
                    encrypted: false,
                    tags: Vec::new(),
                    media_kind: crate::cloud::ArtifactKind::Screenshot,
                },
                expires_at: None,
                version,
            },
        );
        let artifact = crate::cloud::FinalizedArtifact::screenshot_png(
            one_pixel_png(),
            "capture.png".to_owned(),
        )
        .expect("a nonempty PNG is a valid artifact");

        worker.upload(CardId(60), None, version, &artifact, 7);

        assert!(matches!(
            outcome_rx.try_recv(),
            Ok(Outcome::UploadDone { .. })
        ));
        assert!(
            wakes.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "a successful upload must wake the reactive event loop so its \
             outcome is drained promptly rather than waiting on an unrelated repaint"
        );
    }

    #[test]
    fn a_refused_upload_wakes_the_window_event_loop() {
        // Round 8, Finding #5, refusal side: the "cloud" feature is off by
        // default in this workspace's test build, so a fresh (uncached)
        // upload deterministically refuses without any network call --
        // this exercises the same wake requirement on the refusal path.
        let (outcomes, outcome_rx) = std::sync::mpsc::channel();
        let (history, _history_rx) = std::sync::mpsc::channel();
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&wakes);
        let waker: SurfaceWaker = Arc::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        let mut worker = UploadWorker::new(
            outcomes,
            Some(waker),
            crate::cloud::ShareCancellation::default(),
            history,
        );
        let version = ShareVersion {
            generation: Some(2),
            revision: 2,
        };
        let artifact = crate::cloud::FinalizedArtifact::screenshot_png(
            one_pixel_png(),
            "capture.png".to_owned(),
        )
        .expect("a nonempty PNG is a valid artifact");

        worker.upload(CardId(61), None, version, &artifact, 8);

        assert!(matches!(
            outcome_rx.try_recv(),
            Ok(Outcome::UploadRefused { .. })
        ));
        assert!(
            wakes.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "a refused upload must also wake the reactive event loop, not \
             just a successful one"
        );
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
        assert!(pipeline.post(Job::Copy {
            card: CardId(404),
            action: 1,
        }));

        match wait_for(&pipeline) {
            Some(Outcome::OutputRefused {
                card,
                error,
                action,
                ..
            }) => {
                assert_eq!(card, CardId(404));
                assert_eq!(action, 1);
                assert!(error.to_string().contains("404"), "{error}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn saving_a_card_that_was_never_captured_is_refused_too() {
        let pipeline = start_pipeline();
        assert!(pipeline.post(Job::Save {
            card: CardId(7),
            action: 2,
        }));

        match wait_for(&pipeline) {
            Some(Outcome::OutputRefused { card, action, .. }) => {
                assert_eq!(card, CardId(7));
                assert_eq!(action, 2);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn uploading_a_card_that_was_never_captured_is_refused_too() {
        let pipeline = start_pipeline();
        assert!(pipeline.post(Job::Upload {
            card: CardId(8),
            action: 1,
        }));

        match wait_for(&pipeline) {
            Some(Outcome::UploadRefused { card, action, .. }) => {
                assert_eq!(card, CardId(8));
                assert_eq!(
                    action, 1,
                    "a refusal must report the exact action id it answers for"
                );
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn upload_is_forwarded_without_running_network_work_on_the_capture_worker() {
        // The capture worker owns the store connection and the shutter path, so
        // the only thing an Upload may do on it is hand the bytes to a separate
        // worker. The exact revision travels with them: a share must never be
        // attributed to pixels the user did not approve.
        let encoded = one_pixel_png();
        let (mut worker, outcome_rx, upload_rx) = worker_holding_with_uploads(
            CardId(9),
            Cached {
                bytes: CaptureBytes {
                    generation: Some(4),
                    revision: 7,
                    full: Arc::new(encoded.clone()),
                    preview: None,
                },
                editor_source: None,
                rendered: None,
                provenance: Provenance::Region,
                target: CaptureTarget::AllDisplays,
                scale: ScaleFactor::new(1.0),
                color_space: ColorSpace::Srgb,
                capture_id: None,
            },
        );

        worker.upload(CardId(9), 5);

        let UploadJob::Share {
            card,
            artifact,
            version,
            action,
            ..
        } = upload_rx.try_recv().unwrap()
        else {
            panic!("capture worker did not forward the upload")
        };
        assert_eq!(card, CardId(9));
        assert_eq!(artifact.bytes(), encoded.as_slice());
        assert_eq!(artifact.content_type(), "image/png");
        assert_eq!(version.generation, Some(4));
        assert_eq!(version.revision, 7);
        assert_eq!(
            action, 5,
            "the action id must travel from the capture worker to the upload worker unchanged"
        );
        assert!(matches!(
            outcome_rx.try_recv(),
            Ok(Outcome::Started {
                card: CardId(9),
                ..
            })
        ));
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
            scrolling_cancellation: AtomicCancellation::default(),
            uploads: channel().0,
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
    fn ready_capture_reports_when_retention_immediately_evicts_its_source() {
        let root = scratch("ready-capture-retention-truth");
        let store = SqliteStore::open_ephemeral(&root).expect("ephemeral history");
        let (outcomes, _outcome_rx) = channel();
        let mut worker = Worker {
            outcomes,
            scrolling_cancellation: AtomicCancellation::default(),
            uploads: channel().0,
            waker: None,
            selector: Arc::new(RefusingSelector),
            store: Some(store),
            vault: CaptureVault::new(),
            pin_generations: HashMap::new(),
            retention_policy: RetentionPolicy {
                max_image_bytes: 0,
                max_image_age: scrozz_store::RetentionWindow::Forever,
            },
            derived_documents: HashMap::new(),
            analysis_cache: Arc::new(Mutex::new(HashMap::new())),
        };

        let ready = worker
            .finish_capture(
                CaptureKind::Window,
                CardId(46),
                sample_capture(Provenance::Window),
                &no_after_capture_actions(),
            )
            .expect("capture remains available in the live vault");

        assert!(ready.card.capture_id.is_some());
        assert!(!ready.retained_elsewhere);
        assert!(!ready.exported);
        drop(worker);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_pin_pixels_surface_an_error_without_clearing_recoverable_state() {
        let root = scratch("missing-pin-pixels");
        let store = SqliteStore::open_ephemeral(&root).expect("ephemeral history");
        let (outcomes, _outcome_rx) = channel();
        let vault = CaptureVault::new();
        let mut worker = Worker {
            outcomes,
            scrolling_cancellation: AtomicCancellation::default(),
            uploads: channel().0,
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
            scrolling_cancellation: AtomicCancellation::default(),
            uploads: channel().0,
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
            scrolling_cancellation: AtomicCancellation::default(),
            uploads: channel().0,
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
