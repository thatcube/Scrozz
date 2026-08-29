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

use scrozz_annotate::Document;
use scrozz_core::{Capture, CaptureRequest, CaptureTarget, CursorMode, Error as CoreError};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, SystemClipboard};
use scrozz_store::{
    CaptureId, CaptureSharing, History, NewCapture, RemoteObjectId, ShareProvider, ShareTag,
    ShareUrl, SharedMediaKind, SqliteStore, Timestamp,
};

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
pub(crate) enum Job {
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
    /// Upload a card's capture and copy the returned link.
    Upload(CardId),
    /// Replace the bytes all future actions use with a newly finalized revision.
    ReplaceFinalized {
        /// Card whose editor/recorder completed.
        card: CardId,
        /// Exact encoded output after destructive edits.
        artifact: crate::cloud::FinalizedArtifact,
    },
    /// Persist successful remote metadata on the store-owning worker.
    RememberShare {
        capture_id: CaptureId,
        sharing: RememberedShare,
    },
    /// Forget a card's cached bytes. The card itself is the surface's business.
    Release(CardId),
    /// Finish, so the thread can be joined.
    Stop,
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
    /// A capture succeeded and is ready to show.
    Ready(Box<Card>),
    /// A capture failed. The main thread says why and shows nothing.
    Failed {
        /// Which card was expected.
        card: CardId,
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
    uploads: Sender<UploadJob>,
    outcomes: Receiver<Outcome>,
    worker: Option<JoinHandle<()>>,
    upload_worker: Option<JoinHandle<()>>,
    upload_cancellation: crate::cloud::ShareCancellation,
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
        let (uploads, upload_rx) = channel();
        let (outcome_tx, outcomes) = channel();
        let upload_cancellation = crate::cloud::ShareCancellation::default();
        let upload_cancel = upload_cancellation.clone();
        let upload_outcomes = outcome_tx.clone();
        let upload_history = jobs.clone();
        let upload_worker = std::thread::Builder::new()
            .name("scrozz-upload".to_owned())
            .spawn(move || {
                UploadWorker::new(upload_outcomes, upload_cancel, upload_history).run(&upload_rx);
            })
            .map_err(|err| {
                CliError::Core(CoreError::Platform(format!(
                    "could not start the upload worker: {err}"
                )))
            })?;

        let capture_uploads = uploads.clone();
        let worker = match std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || Worker::new(outcome_tx, capture_uploads).run(&job_rx))
        {
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

        Ok(Self {
            jobs,
            uploads,
            outcomes,
            worker: Some(worker),
            upload_worker: Some(upload_worker),
            upload_cancellation,
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

    /// Installs a new exact editor/recording revision for subsequent actions.
    pub fn replace_finalized(
        &self,
        card: CardId,
        artifact: crate::cloud::FinalizedArtifact,
    ) -> bool {
        self.post(Job::ReplaceFinalized { card, artifact })
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
        self.upload_cancellation.cancel();
        let _ = self.uploads.send(UploadJob::Stop);
        if let Some(worker) = self.upload_worker.take() {
            let _ = worker.join();
        }
        // The upload worker can enqueue a final history update. Stop the
        // store-owning worker only after uploads are fully drained.
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
    artifact: crate::cloud::FinalizedArtifact,
    capture_id: Option<CaptureId>,
    revision: u64,
}

struct Worker {
    outcomes: Sender<Outcome>,
    uploads: Sender<UploadJob>,
    store: Option<SqliteStore>,
    cache: HashMap<CardId, Cached>,
}

impl Worker {
    fn new(outcomes: Sender<Outcome>, uploads: Sender<UploadJob>) -> Self {
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
            uploads,
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
                Job::Upload(card) => self.upload(card),
                Job::ReplaceFinalized { card, artifact } => {
                    self.replace_finalized(card, artifact);
                }
                Job::RememberShare {
                    capture_id,
                    sharing,
                } => self.remember_share(&capture_id, sharing),
                Job::Release(card) => {
                    self.cache.remove(&card);
                    let _ = self.uploads.send(UploadJob::Release(card));
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

        let artifact = crate::cloud::FinalizedArtifact::screenshot_png(
            bytes,
            format!("Screenshot-{}.png", card.0),
        )?;
        self.cache.insert(
            card,
            Cached {
                artifact,
                capture_id: capture_id.clone(),
                revision: 1,
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
            upload_available: false,
            upload_unavailable_reason: None,
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
        // The round trip through PNG is deliberate — see the module docs — and
        // is also what will make "copy" work for a card whose capture arrived
        // over IPC, where the worker never held a `Frame` at all.
        let result = self
            .cached(card, "copy")
            .and_then(|cached| Ok(scrozz_export::decode(cached.artifact.bytes())?))
            .and_then(|frame| {
                SystemClipboard::new().write_image_reporting(&frame)?;
                Ok("copied to the clipboard".to_owned())
            });
        self.answer(card, result);
    }

    fn save(&mut self, card: CardId) {
        let result = self.cached(card, "save").and_then(|cached| {
            let path = crate::output::export_default(cached.artifact.bytes())?;
            Ok(format!("saved to {}", path.display()))
        });
        self.answer(card, result);
    }

    fn upload(&mut self, card: CardId) {
        let result = self.cached(card, "upload").and_then(|cached| {
            self.uploads
                .send(UploadJob::Share {
                    card,
                    capture_id: cached.capture_id.clone(),
                    revision: cached.revision,
                    artifact: cached.artifact.clone(),
                })
                .map_err(|_| {
                    CliError::Core(CoreError::Platform("the upload worker has gone".to_owned()))
                })
        });
        match result {
            Ok(()) => {
                let _ = self.outcomes.send(Outcome::Started {
                    card,
                    detail: "upload queued".to_owned(),
                });
            }
            Err(error) => self.answer(card, Err(error)),
        }
    }

    fn replace_finalized(&mut self, card: CardId, artifact: crate::cloud::FinalizedArtifact) {
        let result = self
            .cache
            .get_mut(&card)
            .ok_or_else(|| CliError::usage(format!("{card} has no capture revision to replace")));
        match result {
            Ok(cached) => {
                cached.artifact = artifact;
                cached.revision = cached.revision.saturating_add(1);
                let _ = self.outcomes.send(Outcome::Done {
                    card,
                    detail: format!("finalized revision {} is ready", cached.revision),
                });
            }
            Err(error) => self.answer(card, Err(error)),
        }
    }

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

enum UploadJob {
    Share {
        card: CardId,
        capture_id: Option<CaptureId>,
        revision: u64,
        artifact: crate::cloud::FinalizedArtifact,
    },
    Release(CardId),
    Stop,
}

struct UploadWorker {
    outcomes: Sender<Outcome>,
    cancellation: crate::cloud::ShareCancellation,
    history: Sender<Job>,
    links: HashMap<CardId, CachedShare>,
}

struct CachedShare {
    shared: crate::cloud::Shared,
    expires_at: Option<SystemTime>,
    revision: u64,
}

impl CachedShare {
    fn new(shared: crate::cloud::Shared, revision: u64) -> Self {
        let expires_at = shared.expires_at;
        Self {
            shared,
            expires_at,
            revision,
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
        cancellation: crate::cloud::ShareCancellation,
        history: Sender<Job>,
    ) -> Self {
        Self {
            outcomes,
            cancellation,
            history,
            links: HashMap::new(),
        }
    }

    fn run(mut self, jobs: &Receiver<UploadJob>) {
        while let Ok(job) = jobs.recv() {
            match job {
                UploadJob::Share {
                    card,
                    capture_id,
                    revision,
                    artifact,
                } => {
                    if self.cancellation.is_cancelled() {
                        break;
                    }
                    self.upload(card, capture_id, revision, &artifact);
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
        revision: u64,
        artifact: &crate::cloud::FinalizedArtifact,
    ) {
        if self
            .links
            .get(&card)
            .is_some_and(|shared| shared.is_expired() || shared.revision != revision)
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
                    self.links.insert(card, CachedShare::new(shared, revision));
                }
                Err(error) => {
                    let _ = self.outcomes.send(Outcome::Refused { card, error });
                    return;
                }
            }
        }
        let Some(shared) = self.links.get(&card) else {
            let _ = self.outcomes.send(Outcome::Refused {
                card,
                error: CliError::Core(CoreError::Platform(
                    "the upload completed without a retained share link".to_owned(),
                )),
            });
            return;
        };
        let outcome = match SystemClipboard::new().write_text(&shared.shared.url) {
            Ok(()) => Outcome::Done {
                card,
                detail: "uploaded and copied the private share link".to_owned(),
            },
            Err(error) => Outcome::Refused {
                card,
                error: CliError::Core(CoreError::Platform(format!(
                    "the upload succeeded, but its link could not be copied: {error}. \
                     Press Upload again to retry the clipboard; Scrozz reuses the object while \
                     its signed URL remains valid"
                ))),
            },
        };
        let _ = self.outcomes.send(outcome);
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
            revision: 1,
        };
        assert!(expiring.is_expired());

        let public = CachedShare {
            expires_at: None,
            ..expiring
        };
        assert!(!public.is_expired());
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
    fn uploading_a_card_that_was_never_captured_is_refused_too() {
        let pipeline = Pipeline::start().expect("the worker should start");
        assert!(pipeline.post(Job::Upload(CardId(8))));

        match wait_for(&pipeline) {
            Some(Outcome::Refused { card, .. }) => assert_eq!(card, CardId(8)),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn upload_is_forwarded_without_running_network_work_on_the_capture_worker() {
        let (outcome_tx, outcome_rx) = channel();
        let (upload_tx, upload_rx) = channel();
        let mut worker = Worker {
            outcomes: outcome_tx,
            uploads: upload_tx,
            store: None,
            cache: HashMap::from([(
                CardId(9),
                Cached {
                    artifact: crate::cloud::FinalizedArtifact::screenshot_png(
                        b"encoded-capture".to_vec(),
                        "capture.png",
                    )
                    .unwrap(),
                    capture_id: None,
                    revision: 1,
                },
            )]),
        };

        worker.upload(CardId(9));

        let UploadJob::Share {
            card,
            artifact,
            revision,
            ..
        } = upload_rx.try_recv().unwrap()
        else {
            panic!("capture worker did not forward the upload")
        };
        assert_eq!(card, CardId(9));
        assert_eq!(artifact.bytes(), b"encoded-capture");
        assert_eq!(revision, 1);
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
