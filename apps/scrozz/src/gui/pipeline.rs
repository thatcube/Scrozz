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
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    thread::JoinHandle,
    time::{Duration, SystemTime},
};

use scrozz_annotate::Document;
use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, CursorMode, Display,
    Error as CoreError, Window, WindowId, WindowSelection,
};
use scrozz_export::{Encoder, NamingContext, SystemClipboard};
use scrozz_stitch::{AtomicCancellation, CancelAction, Progress};
use scrozz_store::{CaptureId, History, NewCapture, SqliteStore};

use crate::{
    fault::{CliError, CliResult},
    gui::{
        action::CaptureKind,
        card::{Card, CardId, THUMBNAIL_MAX_EDGE, Thumbnail},
    },
    output::CaptureOutput,
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
    cancellations: Arc<Mutex<HashSet<CardId>>>,
    stopped: Receiver<()>,
    worker: Option<JoinHandle<()>>,
    next_card: u64,
    cancellation: AtomicCancellation,
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
        let (stopped_tx, stopped) = channel();
        let cancellations = Arc::new(Mutex::new(HashSet::new()));
        let worker_cancellations = Arc::clone(&cancellations);
        let cancellation = AtomicCancellation::default();
        let worker_cancellation = cancellation.clone();

        let worker = std::thread::Builder::new()
            .name("scrozz-capture".to_owned())
            .spawn(move || {
                Worker::new(outcome_tx, worker_cancellations, worker_cancellation).run(&job_rx);
                let _ = stopped_tx.send(());
            })
            .map_err(|err| {
                CliError::Core(CoreError::Platform(format!(
                    "could not start the capture worker: {err}"
                )))
            })?;

        Ok(Self {
            jobs,
            outcomes,
            cancellations,
            stopped,
            worker: Some(worker),
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

    /// Takes one finished piece of work, if there is one. Never blocks.
    pub fn poll(&self) -> Option<Outcome> {
        self.outcomes.try_recv().ok()
    }

    /// Stops the worker and waits for it.
    ///
    /// Called from `Drop`, but exposed so a host can shut down deterministically
    /// rather than at an unspecified point during teardown.
    pub fn stop(&mut self) {
        self.cancellation.cancel(CancelAction::Abort);
        let _ = self.jobs.send(Job::Stop);
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
    output: CaptureOutput,
    naming: NamingContext,
    capture_id: Option<CaptureId>,
}

struct Worker {
    outcomes: Sender<Outcome>,
    store: Option<SqliteStore>,
    cache: HashMap<CardId, Cached>,
    window_pickers: HashMap<CardId, Box<dyn CaptureBackend>>,
    cancellations: Arc<Mutex<HashSet<CardId>>>,
    cancellation: AtomicCancellation,
}

impl Worker {
    fn new(
        outcomes: Sender<Outcome>,
        cancellations: Arc<Mutex<HashSet<CardId>>>,
        cancellation: AtomicCancellation,
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
        }
    }

    fn run(mut self, jobs: &Receiver<Job>) {
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
                Job::Release(card) => {
                    self.cache.remove(&card);
                }
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

        self.cache.insert(
            card,
            Cached {
                bytes,
                output,
                naming: NamingContext {
                    width: capture.frame.width(),
                    height: capture.frame.height(),
                    ..NamingContext::now()
                },
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
            source_app: capture.source_app,
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
            let path = cached.output.export(&cached.bytes, &cached.naming)?;
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

fn selectable_windows(backend: &dyn CaptureBackend) -> CliResult<Vec<Window>> {
    let mut windows = backend.windows()?;
    windows.retain(|window| !scrozz_ui::picker::is_scrozz_window(window));
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

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
    fn cancelling_a_window_card_is_visible_before_the_worker_receives_it() {
        let mut pipeline = Pipeline::start().expect("the worker should start");
        let card = pipeline.allocate();

        assert!(pipeline.cancel_window(card));
        assert!(pipeline.is_window_cancelled(card));
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
    fn window_output_keeps_native_edges_stride_and_alpha_without_compositing() {
        let (outcomes, _outcome_rx) = std::sync::mpsc::channel();
        let mut worker = Worker {
            outcomes,
            store: None,
            cache: HashMap::new(),
            window_pickers: HashMap::new(),
            cancellations: Arc::new(Mutex::new(HashSet::new())),
            cancellation: AtomicCancellation::default(),
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
