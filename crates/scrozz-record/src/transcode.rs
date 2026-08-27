//! Native transcoder contracts and deterministic synthetic jobs.

use std::{
    fs::{File, OpenOptions},
    hash::{BuildHasher as _, Hasher as _, RandomState},
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};

use scrozz_core::{Error, Result};
use scrozz_export::{
    AnimationFormat, AnimationRepeat, GIF_MIN_FRAME_DELAY, GifAnimationEncoder, GifAnimationStream,
    TimedRgbaFrame,
};

use crate::{
    Recording,
    edit::{EditOutput, EditPlan, VideoDocument},
    media::{DecodedMediaSample, NativeMediaSource},
};

#[cfg(target_os = "macos")]
#[path = "transcode/macos.rs"]
mod platform;
#[cfg(not(target_os = "macos"))]
mod platform {
    use std::{path::Path, sync::atomic::AtomicBool, time::Duration};

    use scrozz_core::{Error, Result};

    use crate::{
        Quality,
        media::{DecodedAudioChunk, DecodedVideoFrame},
    };

    pub(super) const TRANSCODER_NAME: &str = if cfg!(target_os = "windows") {
        "Windows Media Foundation"
    } else if cfg!(target_os = "linux") {
        "linked FFmpeg + VA-API"
    } else {
        "native media framework"
    };

    pub(super) struct VideoWriter;

    impl VideoWriter {
        pub(super) fn new(
            _path: &Path,
            _dimensions: (u32, u32),
            _fps: f64,
            _quality: Quality,
            _audio_channels: u16,
        ) -> Result<Self> {
            Err(Error::Unsupported {
                what: "native video transcoding".to_owned(),
                why: "this target's native video writer backend is not included in this build"
                    .to_owned(),
            })
        }

        pub(super) fn append_video(
            &mut self,
            _frame: &DecodedVideoFrame,
            _source_origin: Duration,
            _cancelled: &AtomicBool,
        ) -> Result<()> {
            unreachable!("an unavailable native writer cannot be constructed")
        }

        pub(super) fn append_audio(
            &mut self,
            _chunk: &DecodedAudioChunk,
            _source_origin: Duration,
            _output_channels: u16,
            _gain: f32,
            _cancelled: &AtomicBool,
        ) -> Result<()> {
            unreachable!("an unavailable native writer cannot be constructed")
        }

        pub(super) const fn video_frames(&self) -> u64 {
            0
        }

        pub(super) const fn media_end(&self) -> Duration {
            Duration::ZERO
        }

        pub(super) fn finish(&mut self, _requested_end: Duration) -> Result<u64> {
            unreachable!("an unavailable native writer cannot be constructed")
        }
    }
}

static TRANSCODE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const STAGING_DIRECTORY_PREFIX: &str = ".scrozz-transcode-";
const PREPARING_DIRECTORY_PREFIX: &str = ".scrozz-transcode-preparing-";
const ACTIVE_MANIFEST: &[u8] = b"SCROZZ-TRANSCODE/1 active\n";
const RETAINED_MANIFEST: &[u8] = b"SCROZZ-TRANSCODE/1 retained\n";

/// Whether a transcode result came from a real or synthetic implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscodeProvenance {
    /// A real media transcoder.
    Native {
        /// Transcoder implementation name.
        transcoder: String,
    },
    /// A deterministic mock or fixture.
    Synthetic {
        /// Synthetic producer name.
        generator: String,
    },
}

impl TranscodeProvenance {
    /// Whether this is real transcoder output.
    #[must_use]
    pub const fn is_native(&self) -> bool {
        matches!(self, Self::Native { .. })
    }
}

/// Finalisation status of a transcode artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscodeCompletion {
    /// Output finished normally.
    Complete,
    /// Usable bytes exist despite a terminal failure.
    Partial {
        /// Actionable terminal failure.
        reason: String,
    },
}

/// Modeled output from a transcode job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscodeOutput {
    /// Intended output path. Mocks do not write it.
    pub path: PathBuf,
    /// Number of bytes the implementation reports as written.
    pub bytes_written: u64,
    /// Real or synthetic provenance.
    pub provenance: TranscodeProvenance,
    /// Complete or salvageable partial output.
    pub completion: TranscodeCompletion,
}

impl TranscodeOutput {
    /// Creates complete native output metadata.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for unusable metadata.
    pub fn native(
        path: impl Into<PathBuf>,
        bytes_written: u64,
        transcoder: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            path.into(),
            bytes_written,
            TranscodeProvenance::Native {
                transcoder: transcoder.into(),
            },
            TranscodeCompletion::Complete,
        )
    }

    /// Creates partial native output metadata.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for unusable metadata.
    pub fn native_partial(
        path: impl Into<PathBuf>,
        bytes_written: u64,
        transcoder: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            path.into(),
            bytes_written,
            TranscodeProvenance::Native {
                transcoder: transcoder.into(),
            },
            TranscodeCompletion::Partial {
                reason: reason.into(),
            },
        )
    }

    /// Creates complete synthetic output metadata.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for unusable metadata.
    pub fn synthetic(
        path: impl Into<PathBuf>,
        bytes_written: u64,
        generator: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            path.into(),
            bytes_written,
            TranscodeProvenance::Synthetic {
                generator: generator.into(),
            },
            TranscodeCompletion::Complete,
        )
    }

    /// Creates partial synthetic output metadata.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for unusable metadata.
    pub fn synthetic_partial(
        path: impl Into<PathBuf>,
        bytes_written: u64,
        generator: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<Self> {
        Self::new(
            path.into(),
            bytes_written,
            TranscodeProvenance::Synthetic {
                generator: generator.into(),
            },
            TranscodeCompletion::Partial {
                reason: reason.into(),
            },
        )
    }

    fn new(
        path: PathBuf,
        bytes_written: u64,
        provenance: TranscodeProvenance,
        completion: TranscodeCompletion,
    ) -> Result<Self> {
        let output = Self {
            path,
            bytes_written,
            provenance,
            completion,
        };
        output.validate()?;
        Ok(output)
    }

    /// Validates modeled output without reading the filesystem.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for an empty path/producer/reason or
    /// zero usable bytes.
    pub fn validate(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Err(Error::InvalidRequest(
                "a transcode output path cannot be empty".to_owned(),
            ));
        }
        if self.bytes_written == 0 {
            return Err(Error::InvalidRequest(
                "transcode output must report at least one written byte".to_owned(),
            ));
        }
        let producer = match &self.provenance {
            TranscodeProvenance::Native { transcoder } => transcoder,
            TranscodeProvenance::Synthetic { generator } => generator,
        };
        if producer.trim().is_empty() {
            return Err(Error::InvalidRequest(
                "transcode provenance must name its producer".to_owned(),
            ));
        }
        if matches!(
            &self.completion,
            TranscodeCompletion::Partial { reason } if reason.trim().is_empty()
        ) {
            return Err(Error::InvalidRequest(
                "partial transcode output must explain its failure".to_owned(),
            ));
        }
        Ok(())
    }

    /// Rejects synthetic output at a user-real boundary.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for synthetic output.
    pub fn require_native(&self) -> Result<&Self> {
        self.validate()?;
        if let TranscodeProvenance::Synthetic { generator } = &self.provenance {
            return Err(Error::InvalidRequest(format!(
                "synthetic transcode output from {generator:?} cannot be used as real media"
            )));
        }
        Ok(self)
    }

    /// Whether this artifact is partial.
    #[must_use]
    pub const fn is_partial(&self) -> bool {
        matches!(self.completion, TranscodeCompletion::Partial { .. })
    }

    /// Terminal reason, when this artifact is partial.
    #[must_use]
    pub fn partial_reason(&self) -> Option<&str> {
        match &self.completion {
            TranscodeCompletion::Complete => None,
            TranscodeCompletion::Partial { reason } => Some(reason),
        }
    }
}

/// Terminal transcode failure, with mandatory partial metadata when bytes exist.
#[derive(Debug, Clone)]
pub struct TranscodeFailure {
    /// Underlying actionable error.
    pub error: Arc<Error>,
    /// Salvageable output, when the transcoder wrote usable bytes.
    pub partial: Option<TranscodeOutput>,
}

/// Current job state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TranscodeStatus {
    /// Work is active at normalized monotonic progress.
    Running {
        /// Completed fraction in `0.0..=1.0`.
        progress: f32,
    },
    /// Complete output was emitted.
    Finished,
    /// A failure event was emitted.
    Failed,
    /// Cancellation was accepted.
    Cancelled,
}

/// One polled transcode update.
#[derive(Debug, Clone)]
pub enum TranscodeEvent {
    /// Monotonic normalized progress.
    Progress(f32),
    /// Complete output.
    Finished(TranscodeOutput),
    /// Terminal failure and any mandatory partial output.
    Failed(TranscodeFailure),
    /// Cancellation completed, retaining usable native output when any exists.
    Cancelled(Option<TranscodeOutput>),
}

/// A transcode job in progress.
pub trait TranscodeJob: Send {
    /// Polls one deterministic update.
    fn poll(&mut self) -> Option<TranscodeEvent>;

    /// Requests cancellation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when the job is already terminal.
    fn cancel(&mut self) -> Result<()>;

    /// Current job state.
    fn status(&self) -> TranscodeStatus;
}

/// Starts media transcode jobs.
pub trait Transcoder: Send + Sync {
    /// Starts a validated edit plan.
    ///
    /// # Errors
    ///
    /// Returns a request or implementation error before any job begins.
    fn start(
        &self,
        document: &VideoDocument,
        plan: &EditPlan,
        output_path: PathBuf,
    ) -> Result<Box<dyn TranscodeJob>>;
}

/// Native asynchronous transcoder for real recording files.
///
/// The implementation never invokes an external executable. It uses the
/// platform media backend selected by [`NativeMediaSource`], and writes GIFs
/// through the bounded-memory encoder in `scrozz-export`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeTranscoder;

impl NativeTranscoder {
    /// Creates the native transcoder.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Platform implementation used for video output.
    #[must_use]
    pub const fn name() -> &'static str {
        platform::TRANSCODER_NAME
    }

    /// Whether this build includes a real native source and writer backend.
    #[must_use]
    pub const fn is_available() -> bool {
        crate::media::native_media_capabilities().video_transcode
    }
}

impl Transcoder for NativeTranscoder {
    fn start(
        &self,
        document: &VideoDocument,
        plan: &EditPlan,
        output_path: PathBuf,
    ) -> Result<Box<dyn TranscodeJob>> {
        document.validate_plan(plan)?;
        let source = NativeMediaSource::open(document.recording().clone())?;
        validate_document_source(document, &source)?;
        validate_output_path(&source, &output_path)?;
        let staged = StagedOutput::new(&output_path, plan.output)?;
        let output_channels = plan.output_audio_channels(source.metadata());
        if output_channels > 2 {
            return Err(Error::Unsupported {
                what: format!("{output_channels}-channel video export"),
                why: "the native Scrozz export path currently supports mono and stereo PCM/AAC"
                    .to_owned(),
            });
        }

        let (events, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let terminal_gate = Arc::new(AtomicU8::new(0));
        let status = Arc::new(Mutex::new(TranscodeStatus::Running { progress: 0.0 }));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_terminal_gate = Arc::clone(&terminal_gate);
        let worker_status = Arc::clone(&status);
        let plan = *plan;
        let worker = std::thread::Builder::new()
            .name("scrozz-native-transcode".to_owned())
            .spawn(move || {
                let terminal = run_native_transcode(
                    &source,
                    plan,
                    staged,
                    &worker_cancelled,
                    &events,
                    &worker_status,
                );
                let terminal = claim_terminal(terminal, &worker_terminal_gate);
                let event = match terminal {
                    WorkerTerminal::Finished(output) => TranscodeEvent::Finished(output),
                    WorkerTerminal::Failed(failure) => TranscodeEvent::Failed(failure),
                    WorkerTerminal::Cancelled(partial) => TranscodeEvent::Cancelled(partial),
                };
                let _ = events.send(event);
            })
            .map_err(|error| {
                Error::Platform(format!("could not start native transcode worker: {error}"))
            })?;

        Ok(Box::new(NativeJob {
            events: receiver,
            cancelled,
            terminal_gate,
            status,
            terminal_reported: false,
            worker: Some(worker),
        }))
    }
}

struct NativeJob {
    events: Receiver<TranscodeEvent>,
    cancelled: Arc<AtomicBool>,
    terminal_gate: Arc<AtomicU8>,
    status: Arc<Mutex<TranscodeStatus>>,
    terminal_reported: bool,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl TranscodeJob for NativeJob {
    fn poll(&mut self) -> Option<TranscodeEvent> {
        match self.events.try_recv() {
            Ok(event) => {
                let terminal_status = match &event {
                    TranscodeEvent::Finished(_) => Some(TranscodeStatus::Finished),
                    TranscodeEvent::Failed(_) => Some(TranscodeStatus::Failed),
                    TranscodeEvent::Cancelled(_) => Some(TranscodeStatus::Cancelled),
                    TranscodeEvent::Progress(_) => None,
                };
                if let Some(status) = terminal_status {
                    set_status(&self.status, status);
                    self.terminal_reported = true;
                    self.join_worker();
                }
                Some(event)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) if !self.terminal_reported => {
                self.terminal_reported = true;
                set_status(&self.status, TranscodeStatus::Failed);
                self.join_worker();
                Some(TranscodeEvent::Failed(TranscodeFailure {
                    error: Arc::new(Error::Platform(
                        "native transcode worker ended without a terminal event".to_owned(),
                    )),
                    partial: None,
                }))
            }
            Err(TryRecvError::Disconnected) => None,
        }
    }

    fn cancel(&mut self) -> Result<()> {
        match self
            .terminal_gate
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                self.cancelled.store(true, Ordering::Release);
                Ok(())
            }
            Err(1) => Err(Error::InvalidRequest(
                "transcode cancellation was already requested".to_owned(),
            )),
            Err(_) => Err(Error::InvalidRequest(
                "cannot cancel a terminal transcode job".to_owned(),
            )),
        }
    }

    fn status(&self) -> TranscodeStatus {
        *self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for NativeJob {
    fn drop(&mut self) {
        if self
            .terminal_gate
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.cancelled.store(true, Ordering::Release);
        }
        self.join_worker();
        while let Ok(event) = self.events.try_recv() {
            match event {
                TranscodeEvent::Finished(output) => tracing::warn!(
                    path = %output.path.display(),
                    "transcode owner dropped before collecting finished output"
                ),
                TranscodeEvent::Failed(failure) => {
                    if let Some(partial) = failure.partial {
                        tracing::warn!(
                            path = %partial.path.display(),
                            "transcode owner dropped before collecting retained partial output"
                        );
                    } else {
                        tracing::error!(error = %failure.error, "transcode failed after its owner dropped");
                    }
                }
                TranscodeEvent::Cancelled(Some(partial)) => tracing::warn!(
                    path = %partial.path.display(),
                    "cancelled transcode retained output after its owner dropped"
                ),
                TranscodeEvent::Cancelled(None) | TranscodeEvent::Progress(_) => {}
            }
        }
    }
}

impl NativeJob {
    fn join_worker(&mut self) {
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("native transcode worker panicked");
        }
    }
}

enum WorkerTerminal {
    Finished(TranscodeOutput),
    Failed(TranscodeFailure),
    Cancelled(Option<TranscodeOutput>),
}

fn claim_terminal(terminal: WorkerTerminal, gate: &AtomicU8) -> WorkerTerminal {
    match terminal {
        WorkerTerminal::Finished(output) => {
            match gate.compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => WorkerTerminal::Finished(output),
                Err(1) => match TranscodeOutput::native_partial(
                    output.path,
                    output.bytes_written,
                    NativeTranscoder::name(),
                    "cancelled by user after output finalization",
                ) {
                    Ok(partial) => {
                        gate.store(2, Ordering::Release);
                        WorkerTerminal::Cancelled(Some(partial))
                    }
                    Err(error) => {
                        gate.store(2, Ordering::Release);
                        failed(error, None)
                    }
                },
                Err(_) => failed(
                    Error::Platform(
                        "native transcode attempted a second terminal transition".to_owned(),
                    ),
                    None,
                ),
            }
        }
        terminal => {
            gate.store(2, Ordering::Release);
            terminal
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ArtifactKind {
    Video,
    Gif,
}

fn run_native_transcode(
    source: &NativeMediaSource,
    plan: EditPlan,
    mut output: StagedOutput,
    cancelled: &AtomicBool,
    events: &Sender<TranscodeEvent>,
    status: &Arc<Mutex<TranscodeStatus>>,
) -> WorkerTerminal {
    if cancelled.load(Ordering::Acquire) {
        return WorkerTerminal::Cancelled(None);
    }
    let mut progress = ProgressEmitter::new(events, status, plan.trim.duration());
    let terminal = match plan.output {
        EditOutput::Video => run_video(source, plan, output.path(), cancelled, &mut progress),
        EditOutput::Animation(AnimationFormat::Gif) => {
            run_gif(source, plan, output.path(), cancelled, &mut progress)
        }
    };
    output.resolve(terminal)
}

struct StagedOutput {
    final_path: PathBuf,
    staging_path: PathBuf,
    directory: PathBuf,
    lock_path: PathBuf,
    lock: Option<File>,
    retained: bool,
}

struct PublishFailure {
    error: Error,
    retained_path: PathBuf,
}

impl StagedOutput {
    fn new(final_path: &Path, output: EditOutput) -> Result<Self> {
        let parent = final_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        cleanup_abandoned_transcodes(parent)?;
        let (preparing, directory) = create_staging_directory(parent)?;
        let mut preparation = PreparingDirectory(Some(preparing.clone()));
        let preparing_lock = preparing.join("owner.lock");
        let mut lock_options = OpenOptions::new();
        lock_options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        lock_options.mode(0o600);
        let mut lock = lock_options.open(&preparing_lock)?;
        lock.lock()?;
        lock.write_all(ACTIVE_MANIFEST)?;
        lock.sync_all()?;
        std::fs::rename(&preparing, &directory)?;
        preparation.0 = Some(directory.clone());
        sync_parent_directory(&directory)?;
        preparation.0 = None;
        let lock_path = directory.join("owner.lock");
        let extension = match output {
            EditOutput::Video => "mp4",
            EditOutput::Animation(format) => format.extension(),
        };
        Ok(Self {
            final_path: final_path.to_owned(),
            staging_path: directory.join(format!("output.{extension}")),
            directory,
            lock_path,
            lock: Some(lock),
            retained: false,
        })
    }

    fn path(&self) -> &Path {
        &self.staging_path
    }

    fn resolve(&mut self, terminal: WorkerTerminal) -> WorkerTerminal {
        match terminal {
            WorkerTerminal::Finished(mut output) => match self.publish() {
                Ok(()) => {
                    output.path = self.final_path.clone();
                    WorkerTerminal::Finished(output)
                }
                Err(publish) => {
                    let reason = format!(
                        "encoded output could not be durably published at {}: {error}; retained output at {}",
                        self.final_path.display(),
                        publish.retained_path.display(),
                        error = publish.error
                    );
                    self.retain_if_staged(&publish.retained_path);
                    match TranscodeOutput::native_partial(
                        &publish.retained_path,
                        output.bytes_written,
                        NativeTranscoder::name(),
                        &reason,
                    ) {
                        Ok(partial) => failed(Error::Storage(reason), Some(partial)),
                        Err(partial_error) => {
                            failed(combine_errors(Error::Storage(reason), partial_error), None)
                        }
                    }
                }
            },
            WorkerTerminal::Failed(mut failure) => {
                if let Some(partial) = failure.partial.as_mut() {
                    match self.publish() {
                        Ok(()) => partial.path = self.final_path.clone(),
                        Err(publish) => {
                            partial.path = publish.retained_path.clone();
                            self.retain_if_staged(&publish.retained_path);
                            failure.error = Arc::new(combine_errors(
                                failure.error.as_ref().clone(),
                                Error::Storage(format!(
                                    "partial output remains at {} because publication at {} failed: {error}",
                                    publish.retained_path.display(),
                                    self.final_path.display(),
                                    error = publish.error
                                )),
                            ));
                        }
                    }
                }
                WorkerTerminal::Failed(failure)
            }
            WorkerTerminal::Cancelled(mut partial) => {
                if let Some(output) = partial.as_mut() {
                    match self.publish() {
                        Ok(()) => output.path = self.final_path.clone(),
                        Err(publish) => {
                            output.path = publish.retained_path.clone();
                            self.retain_if_staged(&publish.retained_path);
                            let prior = output
                                .partial_reason()
                                .unwrap_or("cancelled by user")
                                .to_owned();
                            output.completion = TranscodeCompletion::Partial {
                                reason: format!(
                                    "{}; retained output remains at {} because publication at {} failed: {error}",
                                    prior,
                                    publish.retained_path.display(),
                                    self.final_path.display(),
                                    error = publish.error
                                ),
                            };
                        }
                    }
                }
                WorkerTerminal::Cancelled(partial)
            }
        }
    }

    fn publish(&mut self) -> Result<(), PublishFailure> {
        if let Err(error) = publish_no_replace(&self.staging_path, &self.final_path) {
            return Err(PublishFailure {
                error,
                retained_path: self.staging_path.clone(),
            });
        }
        let durability = sync_parent_directory(&self.final_path);
        self.cleanup_directory();
        durability.map_err(|error| PublishFailure {
            error,
            retained_path: self.final_path.clone(),
        })
    }

    fn retain(&mut self) {
        self.retained = true;
        if let Some(lock) = self.lock.as_mut()
            && let Err(error) = write_manifest(lock, RETAINED_MANIFEST)
        {
            tracing::error!(
                path = %self.lock_path.display(),
                %error,
                "could not persist retained transcode ownership state"
            );
        }
        self.release_lock();
    }

    fn retain_if_staged(&mut self, path: &Path) {
        if path == self.staging_path {
            self.retain();
        }
    }

    fn release_lock(&mut self) {
        self.lock.take();
    }

    fn cleanup_directory(&mut self) {
        self.release_lock();
        for path in [&self.lock_path, &self.staging_path] {
            if let Err(error) = std::fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::error!(path = %path.display(), %error, "could not clean transcode staging file");
            }
        }
        if let Err(error) = std::fs::remove_dir(&self.directory)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::error!(
                path = %self.directory.display(),
                %error,
                "could not remove empty transcode staging directory"
            );
        }
    }
}

struct PreparingDirectory(Option<PathBuf>);

impl Drop for PreparingDirectory {
    fn drop(&mut self) {
        let Some(directory) = self.0.take() else {
            return;
        };
        let _ = std::fs::remove_file(directory.join("owner.lock"));
        let _ = std::fs::remove_dir(directory);
    }
}

impl Drop for StagedOutput {
    fn drop(&mut self) {
        if !self.retained {
            self.cleanup_directory();
        }
    }
}

fn create_staging_directory(parent: &Path) -> Result<(PathBuf, PathBuf)> {
    for _ in 0..1_000 {
        let sequence = TRANSCODE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut entropy = RandomState::new().build_hasher();
        entropy.write_u64(sequence);
        entropy.write_u64(u64::from(std::process::id()));
        entropy.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        );
        let token = format!("{}-{:016x}", std::process::id(), entropy.finish());
        let preparing = parent.join(format!("{PREPARING_DIRECTORY_PREFIX}{token}"));
        let directory = parent.join(format!("{STAGING_DIRECTORY_PREFIX}{token}"));
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match builder.create(&preparing) {
            Ok(()) => return Ok((preparing, directory)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Err(Error::Storage(
        "could not allocate a private transcode staging directory".into(),
    ))
}

fn cleanup_abandoned_transcodes(parent: &Path) -> Result<()> {
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir()
            || !entry
                .file_name()
                .to_string_lossy()
                .starts_with(STAGING_DIRECTORY_PREFIX)
        {
            continue;
        }
        let directory = entry.path();
        let lock_path = directory.join("owner.lock");
        let mut lock = match OpenOptions::new().read(true).write(true).open(&lock_path) {
            Ok(lock) => lock,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Error::Io(error)),
        };
        match lock.try_lock() {
            Ok(()) => {
                let mut manifest = String::new();
                lock.read_to_string(&mut manifest)?;
                if manifest.as_bytes() != ACTIVE_MANIFEST {
                    continue;
                }
                drop(lock);
                for name in ["output.mp4", "output.gif", "owner.lock"] {
                    let path = directory.join(name);
                    if let Err(error) = std::fs::remove_file(&path)
                        && error.kind() != std::io::ErrorKind::NotFound
                    {
                        return Err(Error::Io(error));
                    }
                }
                if let Err(error) = std::fs::remove_dir(&directory)
                    && !matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                    )
                {
                    return Err(Error::Io(error));
                }
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => return Err(Error::Io(error)),
        }
    }
    Ok(())
}

fn write_manifest(file: &mut File, state: &[u8]) -> std::io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(state)?;
    file.sync_all()
}

#[cfg(target_os = "macos")]
fn publish_no_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    unsafe extern "C" {
        fn renamex_np(
            from: *const std::ffi::c_char,
            to: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    const RENAME_EXCL: u32 = 0x0000_0004;
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| Error::InvalidRequest("transcode staging path contains NUL".into()))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| Error::InvalidRequest("transcode destination contains NUL".into()))?;
    if unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(Error::Io(std::io::Error::last_os_error()))
    }
}

#[cfg(target_os = "windows")]
fn publish_no_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::{Win32::Storage::FileSystem::MoveFileW, core::PCWSTR};

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe { MoveFileW(PCWSTR(source.as_ptr()), PCWSTR(destination.as_ptr())) }
        .map_err(|error| Error::Storage(format!("could not publish transcode output: {error}")))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn publish_no_replace(source: &Path, destination: &Path) -> Result<()> {
    std::fs::hard_link(source, destination)?;
    std::fs::remove_file(source)?;
    Ok(())
}

fn run_gif(
    source: &NativeMediaSource,
    plan: EditPlan,
    output_path: &Path,
    cancelled: &AtomicBool,
    progress: &mut ProgressEmitter<'_>,
) -> WorkerTerminal {
    if plan.trim.duration() < GIF_MIN_FRAME_DELAY {
        return failed(
            Error::InvalidRequest(format!(
                "GIF trim duration must be at least {} ms",
                GIF_MIN_FRAME_DELAY.as_millis()
            )),
            None,
        );
    }
    let dimensions = plan.output_dimensions(source.metadata());
    let mut decoder = match source.decoder_with_dimensions(plan.trim, dimensions) {
        Ok(decoder) => decoder,
        Err(error) => return failed(error, None),
    };
    let mut file_options = OpenOptions::new();
    file_options.write(true).create_new(true);
    #[cfg(unix)]
    file_options.mode(0o600);
    let file = match file_options.open(output_path) {
        Ok(file) => file,
        Err(error) => return failed(Error::Io(error), None),
    };
    let speed = match plan.quality {
        crate::Quality::High => 1,
        crate::Quality::Balanced => GifAnimationEncoder::DEFAULT_SPEED,
        crate::Quality::Low => 30,
    };
    let encoder = match GifAnimationEncoder::with_speed(AnimationRepeat::Infinite, speed) {
        Ok(encoder) => encoder,
        Err(error) => return failed_after_cleanup(error, output_path),
    };
    let mut stream = encoder.stream(BufWriter::new(file));
    let mut frame_count = 0_u64;
    let mut pending = None;
    let mut queued = GifFrameQueue::default();
    let mut cursor = plan.trim.start;

    loop {
        if cancelled.load(Ordering::Acquire) {
            decoder.cancel();
            if let Err(error) = flush_gif_frames(&mut stream, &mut queued, &mut frame_count) {
                return finish_failed_gif(
                    error,
                    stream,
                    frame_count,
                    output_path,
                    plan.trim.duration(),
                );
            }
            return finish_cancelled_gif(stream, frame_count, output_path, plan.trim.duration());
        }
        let sample = match decoder.next_sample() {
            Ok(sample) => sample,
            Err(error) => {
                let error = match flush_gif_frames(&mut stream, &mut queued, &mut frame_count) {
                    Ok(()) => error,
                    Err(flush_error) => combine_errors(error, flush_error),
                };
                return finish_failed_gif(
                    error,
                    stream,
                    frame_count,
                    output_path,
                    plan.trim.duration(),
                );
            }
        };
        let Some(sample) = sample else { break };
        let DecodedMediaSample::Video(frame) = sample else {
            continue;
        };
        if let Some(previous) = pending.replace(frame) {
            let boundary = pending
                .as_ref()
                .expect("the current frame was just installed")
                .timestamp
                .clamp(cursor, plan.trim.end);
            if boundary > cursor {
                if let Err(error) = queue_gif_frame(
                    &mut stream,
                    &mut queued,
                    &mut frame_count,
                    TimedRgbaFrame::new(previous.image, boundary - cursor),
                ) {
                    return finish_failed_gif(
                        error,
                        stream,
                        frame_count,
                        output_path,
                        plan.trim.duration(),
                    );
                }
                cursor = boundary;
            }
        }
        progress.emit(cursor.saturating_sub(plan.trim.start));
    }

    let Some(last) = pending else {
        drop(stream);
        return failed_after_cleanup(
            Error::Codec("GIF export decoded no video frames".to_owned()),
            output_path,
        );
    };
    if cursor < plan.trim.end
        && let Err(error) = queue_gif_frame(
            &mut stream,
            &mut queued,
            &mut frame_count,
            TimedRgbaFrame::new(last.image, plan.trim.end - cursor),
        )
    {
        return finish_failed_gif(
            error,
            stream,
            frame_count,
            output_path,
            plan.trim.duration(),
        );
    }
    if let Err(error) = flush_gif_frames(&mut stream, &mut queued, &mut frame_count) {
        return finish_failed_gif(
            error,
            stream,
            frame_count,
            output_path,
            plan.trim.duration(),
        );
    }
    if cancelled.load(Ordering::Acquire) {
        decoder.cancel();
        return finish_cancelled_gif(stream, frame_count, output_path, plan.trim.duration());
    }
    let bytes_written = match finalize_gif(stream, output_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return terminal_failure(error, output_path, ArtifactKind::Gif, plan.trim.duration());
        }
    };
    if cancelled.load(Ordering::Acquire) {
        return terminal_cancelled(output_path, ArtifactKind::Gif, plan.trim.duration(), None);
    }
    if let Err(error) = verify_gif(output_path, dimensions, plan.trim.duration()) {
        return terminal_failure(error, output_path, ArtifactKind::Gif, plan.trim.duration());
    }
    progress.finish();
    match TranscodeOutput::native(output_path, bytes_written, NativeTranscoder::name()) {
        Ok(output) => WorkerTerminal::Finished(output),
        Err(error) => failed(error, None),
    }
}

fn run_video(
    source: &NativeMediaSource,
    plan: EditPlan,
    output_path: &Path,
    cancelled: &AtomicBool,
    progress: &mut ProgressEmitter<'_>,
) -> WorkerTerminal {
    let dimensions = plan.output_dimensions(source.metadata());
    let output_channels = plan.output_audio_channels(source.metadata());
    let mut decoder = match source.decoder_with_dimensions(plan.trim, dimensions) {
        Ok(decoder) => decoder,
        Err(error) => return failed(error, None),
    };
    let mut writer = match platform::VideoWriter::new(
        output_path,
        dimensions,
        source.metadata().fps,
        plan.quality,
        output_channels,
    ) {
        Ok(writer) => writer,
        Err(error) => {
            return terminal_failure(error, output_path, ArtifactKind::Video, Duration::ZERO);
        }
    };

    loop {
        if cancelled.load(Ordering::Acquire) {
            decoder.cancel();
            return finish_cancelled_video(writer, output_path);
        }
        let sample = match decoder.next_sample() {
            Ok(sample) => sample,
            Err(error) => return finish_failed_video(error, writer, output_path),
        };
        let Some(sample) = sample else { break };
        let result = match &sample {
            DecodedMediaSample::Video(frame) => {
                writer.append_video(frame, plan.trim.start, cancelled)
            }
            DecodedMediaSample::Audio(chunk) if output_channels > 0 => writer.append_audio(
                chunk,
                plan.trim.start,
                output_channels,
                plan.audio.effective_gain(),
                cancelled,
            ),
            DecodedMediaSample::Audio(_) => Ok(()),
        };
        if let Err(error) = result {
            if error.is_cancellation() {
                decoder.cancel();
                return finish_cancelled_video(writer, output_path);
            }
            return finish_failed_video(error, writer, output_path);
        }
        let position = sample
            .timestamp()
            .saturating_sub(plan.trim.start)
            .min(plan.trim.duration());
        progress.emit(position);
    }

    if cancelled.load(Ordering::Acquire) {
        decoder.cancel();
        return finish_cancelled_video(writer, output_path);
    }
    let bytes_written = match writer.finish(plan.trim.duration()) {
        Ok(bytes) => bytes,
        Err(error) => {
            return terminal_failure(error, output_path, ArtifactKind::Video, writer.media_end());
        }
    };
    if cancelled.load(Ordering::Acquire) {
        return terminal_cancelled(output_path, ArtifactKind::Video, plan.trim.duration(), None);
    }
    if let Err(error) = verify_video(
        output_path,
        dimensions,
        output_channels,
        plan.trim.duration(),
    ) {
        return terminal_failure(
            error,
            output_path,
            ArtifactKind::Video,
            plan.trim.duration(),
        );
    }
    progress.finish();
    match TranscodeOutput::native(output_path, bytes_written, NativeTranscoder::name()) {
        Ok(output) => WorkerTerminal::Finished(output),
        Err(error) => failed(error, None),
    }
}

#[derive(Default)]
struct GifFrameQueue {
    ready: Option<TimedRgbaFrame>,
    pending: Option<TimedRgbaFrame>,
}

fn queue_gif_frame<W: Write>(
    stream: &mut GifAnimationStream<W>,
    queued: &mut GifFrameQueue,
    frame_count: &mut u64,
    mut frame: TimedRgbaFrame,
) -> Result<()> {
    if frame.delay.is_zero() {
        return Ok(());
    }
    let Some(mut previous) = queued.pending.take() else {
        queued.pending = Some(frame);
        return Ok(());
    };
    if previous.delay >= GIF_MIN_FRAME_DELAY {
        if let Some(ready) = queued.ready.replace(previous) {
            stream.write_frame(ready)?;
            *frame_count = frame_count.saturating_add(1);
        }
        queued.pending = Some(frame);
    } else if frame.delay < GIF_MIN_FRAME_DELAY {
        previous.delay = previous.delay.saturating_add(frame.delay);
        queued.pending = Some(previous);
    } else {
        frame.delay = frame.delay.saturating_add(previous.delay);
        queued.pending = Some(frame);
    }
    Ok(())
}

fn flush_gif_frames<W: Write>(
    stream: &mut GifAnimationStream<W>,
    queued: &mut GifFrameQueue,
    frame_count: &mut u64,
) -> Result<()> {
    let ready = queued.ready.take();
    let pending = queued.pending.take();
    match (ready, pending) {
        (Some(mut ready), Some(mut pending)) if pending.delay < GIF_MIN_FRAME_DELAY => {
            let combined = ready.delay.saturating_add(pending.delay);
            let available = stream.projected_centiseconds(combined)?;
            let reserved_tail = GIF_MIN_FRAME_DELAY;
            let ready_delay = combined.saturating_sub(reserved_tail);
            let ready_ticks = stream.projected_centiseconds(ready_delay)?;
            if available >= 2
                && !ready_delay.is_zero()
                && ready_ticks > 0
                && ready_ticks < available
            {
                ready.delay = ready_delay;
                pending.delay = reserved_tail;
                write_queued_gif_frame(stream, frame_count, ready)?;
                write_queued_gif_frame(stream, frame_count, pending)?;
            } else {
                ready.delay = combined;
                write_queued_gif_frame(stream, frame_count, ready)?;
            }
        }
        (Some(ready), Some(pending)) => {
            write_queued_gif_frame(stream, frame_count, ready)?;
            write_queued_gif_frame(stream, frame_count, pending)?;
        }
        (Some(frame), None) | (None, Some(frame)) => {
            write_queued_gif_frame(stream, frame_count, frame)?;
        }
        (None, None) => {}
    }
    Ok(())
}

fn write_queued_gif_frame<W: Write>(
    stream: &mut GifAnimationStream<W>,
    frame_count: &mut u64,
    frame: TimedRgbaFrame,
) -> Result<()> {
    stream.write_frame(frame)?;
    *frame_count = frame_count.saturating_add(1);
    Ok(())
}

fn finish_failed_gif(
    error: Error,
    stream: GifAnimationStream<BufWriter<File>>,
    frame_count: u64,
    output_path: &Path,
    duration: Duration,
) -> WorkerTerminal {
    let error = if frame_count == 0 {
        drop(stream);
        error
    } else {
        match finalize_gif(stream, output_path) {
            Ok(_) => error,
            Err(finalize) => combine_errors(error, finalize),
        }
    };
    terminal_failure(error, output_path, ArtifactKind::Gif, duration)
}

fn finish_cancelled_gif(
    stream: GifAnimationStream<BufWriter<File>>,
    frame_count: u64,
    output_path: &Path,
    duration: Duration,
) -> WorkerTerminal {
    if frame_count == 0 {
        drop(stream);
        return cancelled_after_cleanup(output_path);
    }
    let finalize_error = finalize_gif(stream, output_path).err();
    terminal_cancelled(output_path, ArtifactKind::Gif, duration, finalize_error)
}

fn finalize_gif(stream: GifAnimationStream<BufWriter<File>>, output_path: &Path) -> Result<u64> {
    let mut writer = stream.finish()?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    let bytes = writer.get_ref().metadata()?.len();
    sync_parent_directory(output_path)?;
    Ok(bytes)
}

fn finish_failed_video(
    error: Error,
    mut writer: platform::VideoWriter,
    output_path: &Path,
) -> WorkerTerminal {
    let duration = writer.media_end();
    let error = if writer.video_frames() == 0 {
        discard_video_writer(writer);
        error
    } else {
        match writer.finish(duration) {
            Ok(_) => error,
            Err(finalize) => combine_errors(error, finalize),
        }
    };
    terminal_failure(error, output_path, ArtifactKind::Video, duration)
}

fn finish_cancelled_video(mut writer: platform::VideoWriter, output_path: &Path) -> WorkerTerminal {
    let duration = writer.media_end();
    if writer.video_frames() == 0 {
        discard_video_writer(writer);
        return cancelled_after_cleanup(output_path);
    }
    let finalize_error = writer.finish(duration).err();
    terminal_cancelled(output_path, ArtifactKind::Video, duration, finalize_error)
}

fn discard_video_writer(writer: platform::VideoWriter) {
    #[cfg(target_os = "macos")]
    drop(writer);
    #[cfg(not(target_os = "macos"))]
    {
        let _writer = writer;
    }
}

fn terminal_failure(
    error: Error,
    output_path: &Path,
    kind: ArtifactKind,
    duration: Duration,
) -> WorkerTerminal {
    let reason = error.to_string();
    match retain_partial(output_path, kind, duration, &reason) {
        Ok(partial) => failed(error, partial),
        Err(retention_error) => failed(combine_errors(error, retention_error), None),
    }
}

fn terminal_cancelled(
    output_path: &Path,
    kind: ArtifactKind,
    duration: Duration,
    finalize_error: Option<Error>,
) -> WorkerTerminal {
    let reason = finalize_error.as_ref().map_or_else(
        || "cancelled by user".to_owned(),
        |error| format!("cancelled by user; finalization also failed: {error}"),
    );
    match retain_partial(output_path, kind, duration, &reason) {
        Ok(partial) => WorkerTerminal::Cancelled(partial),
        Err(error) => failed(error, None),
    }
}

fn retain_partial(
    output_path: &Path,
    kind: ArtifactKind,
    duration: Duration,
    reason: &str,
) -> Result<Option<TranscodeOutput>> {
    let file_size = match std::fs::metadata(output_path) {
        Ok(metadata) if metadata.is_file() && metadata.len() > 0 => metadata.len(),
        Ok(_) => {
            remove_output(output_path)?;
            return Ok(None);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::Io(error)),
    };
    let usable = match kind {
        ArtifactKind::Gif => scrozz_export::inspect_gif_file(output_path).is_ok(),
        ArtifactKind::Video => {
            let duration = duration.max(Duration::from_nanos(1));
            let recording = Recording::native_partial(
                output_path,
                duration.as_secs_f64(),
                NativeTranscoder::name(),
                reason,
            )?;
            NativeMediaSource::open(recording).is_ok()
        }
    };
    if !usable {
        remove_output(output_path)?;
        return Ok(None);
    }
    TranscodeOutput::native_partial(output_path, file_size, NativeTranscoder::name(), reason)
        .map(Some)
}

fn verify_gif(
    output_path: &Path,
    dimensions: (u32, u32),
    expected_duration: Duration,
) -> Result<()> {
    let gif = scrozz_export::inspect_gif_file(output_path)?;
    if (u32::from(gif.width), u32::from(gif.height)) != dimensions {
        return Err(Error::Codec(format!(
            "written GIF is {}x{}, expected {}x{}",
            gif.width, gif.height, dimensions.0, dimensions.1
        )));
    }
    let expected_duration = quantized_gif_duration(expected_duration)?;
    if gif.duration != expected_duration {
        return Err(Error::Codec(format!(
            "written GIF duration {} ms differs from expected {} ms",
            gif.duration.as_millis(),
            expected_duration.as_millis()
        )));
    }
    Ok(())
}

fn quantized_gif_duration(duration: Duration) -> Result<Duration> {
    const CENTISECOND_NANOS: u128 = 10_000_000;
    let centiseconds = duration
        .as_nanos()
        .checked_add(CENTISECOND_NANOS / 2)
        .ok_or_else(|| Error::InvalidRequest("GIF duration overflowed".to_owned()))?
        / CENTISECOND_NANOS;
    let millis = centiseconds
        .checked_mul(10)
        .and_then(|millis| u64::try_from(millis).ok())
        .ok_or_else(|| Error::InvalidRequest("GIF duration exceeds u64 milliseconds".to_owned()))?;
    Ok(Duration::from_millis(millis))
}

fn verify_video(
    output_path: &Path,
    dimensions: (u32, u32),
    audio_channels: u16,
    expected_duration: Duration,
) -> Result<()> {
    let recording = Recording::native(
        output_path,
        expected_duration.as_secs_f64(),
        NativeTranscoder::name(),
    )?;
    let written = NativeMediaSource::open(recording)?;
    let metadata = written.metadata();
    if (metadata.width, metadata.height) != dimensions {
        return Err(Error::Codec(format!(
            "written video is {}x{}, expected {}x{}",
            metadata.width, metadata.height, dimensions.0, dimensions.1
        )));
    }
    if metadata.audio_channels != audio_channels {
        return Err(Error::Codec(format!(
            "written video has {} audio channels, expected {audio_channels}",
            metadata.audio_channels
        )));
    }
    let tolerance = Duration::try_from_secs_f64((1.0 / metadata.fps).max(0.050))
        .unwrap_or(Duration::from_millis(50));
    if written.inspection().duration.abs_diff(expected_duration) > tolerance {
        return Err(Error::Codec(format!(
            "written video duration {:.3} s differs from expected {:.3} s",
            written.inspection().duration.as_secs_f64(),
            expected_duration.as_secs_f64()
        )));
    }
    Ok(())
}

fn validate_document_source(document: &VideoDocument, source: &NativeMediaSource) -> Result<()> {
    let expected = document.metadata();
    let actual = source.metadata();
    if (expected.width, expected.height, expected.audio_channels)
        != (actual.width, actual.height, actual.audio_channels)
        || (expected.fps - actual.fps).abs() > actual.fps.max(1.0) * 0.01
    {
        return Err(Error::InvalidRequest(format!(
            "video document metadata {}x{} @ {:.3} fps / {} channels does not match source {}x{} @ {:.3} fps / {} channels; reopen with VideoDocument::open_native",
            expected.width,
            expected.height,
            expected.fps,
            expected.audio_channels,
            actual.width,
            actual.height,
            actual.fps,
            actual.audio_channels
        )));
    }
    let tolerance = Duration::try_from_secs_f64((1.0 / actual.fps).max(0.050))
        .unwrap_or(Duration::from_millis(50));
    if document.duration().abs_diff(source.inspection().duration) > tolerance {
        return Err(Error::InvalidRequest(format!(
            "video document duration {:.3} s does not match source duration {:.3} s; reopen with VideoDocument::open_native",
            document.duration().as_secs_f64(),
            source.inspection().duration.as_secs_f64()
        )));
    }
    Ok(())
}

fn validate_output_path(source: &NativeMediaSource, output_path: &Path) -> Result<()> {
    if output_path.as_os_str().is_empty() {
        return Err(Error::InvalidRequest(
            "a transcode output path cannot be empty".to_owned(),
        ));
    }
    if output_path == source.path() {
        return Err(Error::InvalidRequest(
            "a transcode cannot overwrite its source recording".to_owned(),
        ));
    }
    if output_path.exists() {
        return Err(Error::InvalidRequest(format!(
            "transcode destination already exists: {}",
            output_path.display()
        )));
    }
    if let Some(parent) = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn failed_after_cleanup(error: Error, output_path: &Path) -> WorkerTerminal {
    match remove_output(output_path) {
        Ok(()) => failed(error, None),
        Err(cleanup) => failed(combine_errors(error, cleanup), None),
    }
}

fn cancelled_after_cleanup(output_path: &Path) -> WorkerTerminal {
    match remove_output(output_path) {
        Ok(()) => WorkerTerminal::Cancelled(None),
        Err(error) => failed(error, None),
    }
}

fn remove_output(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Storage(format!(
            "could not remove unusable transcode output {}: {error}",
            path.display()
        ))),
    }
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn combine_errors(primary: Error, secondary: Error) -> Error {
    Error::Codec(format!("{primary}; additionally, {secondary}"))
}

fn failed(error: Error, partial: Option<TranscodeOutput>) -> WorkerTerminal {
    WorkerTerminal::Failed(TranscodeFailure {
        error: Arc::new(error),
        partial,
    })
}

fn set_status(status: &Mutex<TranscodeStatus>, value: TranscodeStatus) {
    *status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
}

struct ProgressEmitter<'a> {
    events: &'a Sender<TranscodeEvent>,
    status: &'a Arc<Mutex<TranscodeStatus>>,
    duration: Duration,
    last: f32,
}

impl<'a> ProgressEmitter<'a> {
    fn new(
        events: &'a Sender<TranscodeEvent>,
        status: &'a Arc<Mutex<TranscodeStatus>>,
        duration: Duration,
    ) -> Self {
        Self {
            events,
            status,
            duration,
            last: 0.0,
        }
    }

    fn emit(&mut self, position: Duration) {
        let progress =
            (position.as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0) as f32;
        if progress < 1.0 && progress < self.last + 0.01 {
            return;
        }
        self.last = progress.max(self.last);
        set_status(
            self.status,
            TranscodeStatus::Running {
                progress: self.last,
            },
        );
        let _ = self.events.send(TranscodeEvent::Progress(self.last));
    }

    fn finish(&mut self) {
        if self.last < 1.0 {
            self.last = 1.0;
            set_status(self.status, TranscodeStatus::Running { progress: 1.0 });
            let _ = self.events.send(TranscodeEvent::Progress(1.0));
        }
    }
}

#[derive(Debug)]
enum MockOutcome {
    Success { bytes_written: u64 },
    Failure { error: Error, bytes_written: u64 },
}

#[derive(Debug)]
struct MockPlan {
    progress: Vec<f32>,
    outcome: MockOutcome,
}

/// Deterministic transcoder that models output but never writes a file.
#[derive(Debug)]
pub struct MockTranscoder {
    plan: Mutex<Option<MockPlan>>,
}

impl MockTranscoder {
    /// Creates a successful one-job mock.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for non-monotonic/out-of-range progress
    /// or zero output bytes.
    pub fn success(progress: Vec<f32>, bytes_written: u64) -> Result<Self> {
        validate_progress(&progress)?;
        if bytes_written == 0 {
            return Err(Error::InvalidRequest(
                "a successful mock transcode must report output bytes".to_owned(),
            ));
        }
        Ok(Self {
            plan: Mutex::new(Some(MockPlan {
                progress,
                outcome: MockOutcome::Success { bytes_written },
            })),
        })
    }

    /// Creates a failing one-job mock.
    ///
    /// `bytes_written > 0` produces mandatory structured partial output;
    /// `bytes_written == 0` produces no partial artifact.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for non-monotonic/out-of-range
    /// progress.
    pub fn failure(progress: Vec<f32>, error: Error, bytes_written: u64) -> Result<Self> {
        validate_progress(&progress)?;
        Ok(Self {
            plan: Mutex::new(Some(MockPlan {
                progress,
                outcome: MockOutcome::Failure {
                    error,
                    bytes_written,
                },
            })),
        })
    }
}

impl Transcoder for MockTranscoder {
    fn start(
        &self,
        document: &VideoDocument,
        plan: &EditPlan,
        output_path: PathBuf,
    ) -> Result<Box<dyn TranscodeJob>> {
        document.validate_plan(plan)?;
        if output_path.as_os_str().is_empty() {
            return Err(Error::InvalidRequest(
                "a transcode output path cannot be empty".to_owned(),
            ));
        }
        let plan = self
            .plan
            .lock()
            .map_err(|_| Error::Platform("mock transcoder plan lock was poisoned".to_owned()))?
            .take()
            .ok_or_else(|| {
                Error::InvalidRequest(
                    "a deterministic mock transcoder can start its one scripted job only once"
                        .to_owned(),
                )
            })?;
        Ok(Box::new(MockJob {
            output_path,
            progress: plan.progress.into_iter(),
            outcome: Some(plan.outcome),
            status: TranscodeStatus::Running { progress: 0.0 },
            cancellation_event_pending: false,
        }))
    }
}

struct MockJob {
    output_path: PathBuf,
    progress: std::vec::IntoIter<f32>,
    outcome: Option<MockOutcome>,
    status: TranscodeStatus,
    cancellation_event_pending: bool,
}

impl TranscodeJob for MockJob {
    fn poll(&mut self) -> Option<TranscodeEvent> {
        if self.cancellation_event_pending {
            self.cancellation_event_pending = false;
            return Some(TranscodeEvent::Cancelled(None));
        }
        if !matches!(self.status, TranscodeStatus::Running { .. }) {
            return None;
        }
        if let Some(progress) = self.progress.next() {
            self.status = TranscodeStatus::Running { progress };
            return Some(TranscodeEvent::Progress(progress));
        }

        match self.outcome.take()? {
            MockOutcome::Success { bytes_written } => {
                let output = TranscodeOutput {
                    path: self.output_path.clone(),
                    bytes_written,
                    provenance: TranscodeProvenance::Synthetic {
                        generator: "deterministic mock transcoder".to_owned(),
                    },
                    completion: TranscodeCompletion::Complete,
                };
                self.status = TranscodeStatus::Finished;
                Some(TranscodeEvent::Finished(output))
            }
            MockOutcome::Failure {
                error,
                bytes_written,
            } => {
                let reason = error.to_string();
                let partial = (bytes_written > 0).then(|| TranscodeOutput {
                    path: self.output_path.clone(),
                    bytes_written,
                    provenance: TranscodeProvenance::Synthetic {
                        generator: "deterministic mock transcoder".to_owned(),
                    },
                    completion: TranscodeCompletion::Partial { reason },
                });
                self.status = TranscodeStatus::Failed;
                Some(TranscodeEvent::Failed(TranscodeFailure {
                    error: Arc::new(error),
                    partial,
                }))
            }
        }
    }

    fn cancel(&mut self) -> Result<()> {
        if !matches!(self.status, TranscodeStatus::Running { .. }) {
            return Err(Error::InvalidRequest(
                "cannot cancel a terminal transcode job".to_owned(),
            ));
        }
        self.status = TranscodeStatus::Cancelled;
        self.cancellation_event_pending = true;
        Ok(())
    }

    fn status(&self) -> TranscodeStatus {
        self.status
    }
}

fn validate_progress(progress: &[f32]) -> Result<()> {
    let mut previous = 0.0;
    for (index, value) in progress.iter().copied().enumerate() {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(Error::InvalidRequest(format!(
                "transcode progress step {index} is {value}; expected 0.0..=1.0"
            )));
        }
        if value < previous {
            return Err(Error::InvalidRequest(format!(
                "transcode progress regressed from {previous} to {value} at step {index}"
            )));
        }
        previous = value;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    #[cfg(target_os = "macos")]
    use std::{
        sync::atomic::AtomicBool,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        Recording,
        edit::{EditPlan, SourceMetadata, VideoDocument},
    };
    #[cfg(target_os = "macos")]
    use crate::{
        edit::{ChannelBehavior, TrimRange},
        media::{DecodedAudioChunk, DecodedMediaSample, DecodedVideoFrame, NativeMediaSource},
    };
    use image::{AnimationDecoder, codecs::gif::GifDecoder};
    use scrozz_export::RgbaImage;

    use super::*;

    fn fixture() -> (VideoDocument, EditPlan) {
        let document = VideoDocument::open_fixture(
            Recording::synthetic("source.mp4", 4.0, "transcode test").unwrap(),
            SourceMetadata {
                width: 1920,
                height: 1080,
                fps: 30.0,
                audio_channels: 2,
            },
        )
        .unwrap();
        let plan = EditPlan::video(&document).unwrap();
        (document, plan)
    }

    #[test]
    fn success_reports_monotonic_progress_then_synthetic_output() {
        let (document, plan) = fixture();
        let transcoder = MockTranscoder::success(vec![0.1, 0.5, 1.0], 2048).unwrap();
        let mut job = transcoder
            .start(&document, &plan, PathBuf::from("out.mp4"))
            .unwrap();
        let mut progress = Vec::new();
        loop {
            match job.poll().unwrap() {
                TranscodeEvent::Progress(value) => progress.push(value),
                TranscodeEvent::Finished(output) => {
                    assert!(!output.provenance.is_native());
                    assert!(output.require_native().is_err());
                    assert_eq!(output.bytes_written, 2048);
                    break;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(progress, [0.1, 0.5, 1.0]);
        assert_eq!(job.status(), TranscodeStatus::Finished);
        assert!(job.poll().is_none());
    }

    #[test]
    fn cancellation_has_state_and_one_terminal_event() {
        let (document, plan) = fixture();
        let transcoder = MockTranscoder::success(vec![0.2, 0.8], 100).unwrap();
        let mut job = transcoder
            .start(&document, &plan, PathBuf::from("unused.mp4"))
            .unwrap();
        assert!(job.cancel().is_ok());
        assert_eq!(job.status(), TranscodeStatus::Cancelled);
        assert!(matches!(job.poll(), Some(TranscodeEvent::Cancelled(None))));
        assert!(job.poll().is_none());
        assert!(job.cancel().is_err());
    }

    #[test]
    fn failure_with_written_bytes_mandates_structured_partial_output() {
        let (document, plan) = fixture();
        let transcoder = MockTranscoder::failure(
            vec![0.25, 0.75],
            Error::Codec("mux trailer failed".to_owned()),
            8192,
        )
        .unwrap();
        let mut job = transcoder
            .start(&document, &plan, PathBuf::from("partial.mp4"))
            .unwrap();
        assert!(matches!(job.poll(), Some(TranscodeEvent::Progress(0.25))));
        assert!(matches!(job.poll(), Some(TranscodeEvent::Progress(0.75))));
        let Some(TranscodeEvent::Failed(failure)) = job.poll() else {
            panic!("expected failure");
        };
        assert!(failure.error.to_string().contains("mux trailer"));
        let partial = failure
            .partial
            .expect("written bytes require partial output");
        assert!(partial.is_partial());
        assert_eq!(partial.bytes_written, 8192);
        assert!(!partial.provenance.is_native());
        assert_eq!(job.status(), TranscodeStatus::Failed);
    }

    #[test]
    fn failure_without_written_bytes_has_no_fake_partial() {
        let (document, plan) = fixture();
        let transcoder =
            MockTranscoder::failure(vec![], Error::Codec("open failed".to_owned()), 0).unwrap();
        let mut job = transcoder
            .start(&document, &plan, PathBuf::from("never-created.mp4"))
            .unwrap();
        let Some(TranscodeEvent::Failed(failure)) = job.poll() else {
            panic!("expected failure");
        };
        assert!(failure.partial.is_none());
    }

    #[test]
    fn invalid_progress_is_rejected_before_a_job_exists() {
        assert!(MockTranscoder::success(vec![0.5, 0.4], 1).is_err());
        assert!(MockTranscoder::success(vec![f32::NAN], 1).is_err());
    }

    #[test]
    fn gif_scheduler_keeps_motion_above_one_hundred_fps() {
        let encoder = GifAnimationEncoder::with_repeat(AnimationRepeat::Once);
        let mut stream = encoder.stream(Vec::new());
        let mut queued = GifFrameQueue::default();
        let mut frame_count = 0;
        for index in 0..12_u8 {
            queue_gif_frame(
                &mut stream,
                &mut queued,
                &mut frame_count,
                TimedRgbaFrame::new(
                    RgbaImage {
                        width: 1,
                        height: 1,
                        data: vec![index, 0, 0, 255],
                    },
                    Duration::from_millis(4),
                ),
            )
            .unwrap();
        }
        flush_gif_frames(&mut stream, &mut queued, &mut frame_count).unwrap();
        let bytes = stream.finish().unwrap();
        let frames = GifDecoder::new(std::io::Cursor::new(bytes))
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();
        assert_eq!(frame_count, 4);
        assert_eq!(frames.len(), 4);
        assert_ne!(
            frames.first().unwrap().buffer(),
            frames.last().unwrap().buffer()
        );
        let total_millis: u32 = frames
            .iter()
            .map(|frame| frame.delay().numer_denom_ms().0)
            .sum();
        assert_eq!(total_millis, 50);
    }

    #[test]
    fn gif_scheduler_preserves_a_short_tail_when_the_pair_is_representable() {
        let encoder = GifAnimationEncoder::with_repeat(AnimationRepeat::Once);
        let mut stream = encoder.stream(Vec::new());
        let mut queued = GifFrameQueue::default();
        let mut frame_count = 0;
        for (index, delay) in [33, 33, 4].into_iter().enumerate() {
            queue_gif_frame(
                &mut stream,
                &mut queued,
                &mut frame_count,
                TimedRgbaFrame::new(
                    RgbaImage {
                        width: 1,
                        height: 1,
                        data: vec![index as u8, 0, 0, 255],
                    },
                    Duration::from_millis(delay),
                ),
            )
            .unwrap();
        }
        flush_gif_frames(&mut stream, &mut queued, &mut frame_count).unwrap();
        let bytes = stream.finish().unwrap();
        let frames = GifDecoder::new(std::io::Cursor::new(bytes))
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();

        assert_eq!(frame_count, 3);
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.delay().numer_denom_ms().0)
                .sum::<u32>(),
            70
        );
        assert_eq!(frames[2].buffer().get_pixel(0, 0).0, [2, 0, 0, 255]);
    }

    #[test]
    fn gif_scheduler_handles_an_odd_short_frame_count() {
        let encoder = GifAnimationEncoder::with_repeat(AnimationRepeat::Once);
        let mut stream = encoder.stream(Vec::new());
        let mut queued = GifFrameQueue::default();
        let mut frame_count = 0;
        for index in 0..5_u8 {
            queue_gif_frame(
                &mut stream,
                &mut queued,
                &mut frame_count,
                TimedRgbaFrame::new(
                    RgbaImage {
                        width: 1,
                        height: 1,
                        data: vec![index, 0, 0, 255],
                    },
                    Duration::from_millis(4),
                ),
            )
            .unwrap();
        }
        flush_gif_frames(&mut stream, &mut queued, &mut frame_count).unwrap();
        let bytes = stream.finish().unwrap();
        let frames = GifDecoder::new(std::io::Cursor::new(bytes))
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();

        assert_eq!(frames.len(), 2);
        assert_ne!(frames[0].buffer(), frames[1].buffer());
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.delay().numer_denom_ms().0)
                .sum::<u32>(),
            20
        );
    }

    #[test]
    fn gif_scheduler_uses_cumulative_rounding_for_four_short_frames() {
        let encoder = GifAnimationEncoder::with_repeat(AnimationRepeat::Once);
        let mut stream = encoder.stream(Vec::new());
        let mut queued = GifFrameQueue::default();
        let mut frame_count = 0;
        for index in 0..4_u8 {
            queue_gif_frame(
                &mut stream,
                &mut queued,
                &mut frame_count,
                TimedRgbaFrame::new(
                    RgbaImage {
                        width: 1,
                        height: 1,
                        data: vec![index, 0, 0, 255],
                    },
                    Duration::from_millis(4),
                ),
            )
            .unwrap();
        }
        flush_gif_frames(&mut stream, &mut queued, &mut frame_count).unwrap();
        let frames = GifDecoder::new(std::io::Cursor::new(stream.finish().unwrap()))
            .unwrap()
            .into_frames()
            .collect_frames()
            .unwrap();

        assert_eq!(frames.len(), 2);
        assert_ne!(frames[0].buffer(), frames[1].buffer());
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.delay().numer_denom_ms().0)
                .sum::<u32>(),
            20
        );
    }

    #[test]
    fn source_plan_is_revalidated_at_start() {
        let (document, mut plan) = fixture();
        plan.trim.end = document.duration() + Duration::from_secs(1);
        let transcoder = MockTranscoder::success(vec![], 1).unwrap();
        let result = transcoder.start(&document, &plan, PathBuf::from("out.mp4"));
        assert!(result.is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn staged_output_publishes_without_replacing_a_race_winner() {
        let directory = TestDirectory::new("staged-publish");
        let final_path = directory.path.join("edited.mp4");
        let mut staged = StagedOutput::new(&final_path, EditOutput::Video).unwrap();
        std::fs::write(staged.path(), b"encoded").unwrap();
        std::fs::write(&final_path, b"race winner").unwrap();
        let output = TranscodeOutput::native(staged.path(), 7, NativeTranscoder::name()).unwrap();

        let WorkerTerminal::Failed(failure) = staged.resolve(WorkerTerminal::Finished(output))
        else {
            panic!("publication collision must fail with retained output");
        };
        let partial = failure.partial.expect("encoded staging file is retained");
        assert!(partial.is_partial());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"race winner");
        assert_eq!(std::fs::read(&partial.path).unwrap(), b"encoded");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_artifact_is_inspected_and_decoded_from_real_media() {
        let directory = TestDirectory::new("inspect");
        let recording = write_native_fixture(&directory.path.join("source.mp4"));
        let source = NativeMediaSource::open(recording.clone()).expect("inspect native MP4");
        assert_eq!(
            (source.metadata().width, source.metadata().height),
            (96, 64)
        );
        assert_eq!(source.metadata().audio_channels, 2);
        assert!(source.inspection().file_size_bytes > 0);
        let synthetic = Recording::synthetic(
            recording.path.clone(),
            recording.duration_secs,
            "synthetic wrapper",
        )
        .unwrap();
        assert!(NativeMediaSource::open(synthetic).is_err());

        let document = VideoDocument::open_native(recording).expect("open native document");
        let range = crate::edit::TrimRange::full(document.duration()).unwrap();
        let mut decoder = source.decoder(range).expect("open native decoder");
        let mut saw_video = false;
        let mut saw_audio = false;
        for _ in 0..64 {
            match decoder.next_sample().expect("decode native sample") {
                Some(DecodedMediaSample::Video(frame)) => {
                    assert_eq!((frame.image.width, frame.image.height), (96, 64));
                    assert_eq!(frame.image.data.len(), 96 * 64 * 4);
                    saw_video = true;
                }
                Some(DecodedMediaSample::Audio(chunk)) => {
                    assert_eq!(chunk.channels, 2);
                    assert!(!chunk.samples.is_empty());
                    saw_audio = true;
                }
                None => break,
            }
            if saw_video && saw_audio {
                break;
            }
        }
        assert!(saw_video && saw_audio);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_transcoder_writes_trimmed_resized_mono_muted_and_gif_artifacts() {
        let directory = TestDirectory::new("transcode");
        let recording = write_native_fixture(&directory.path.join("source.mp4"));
        let document = VideoDocument::open_native(recording).expect("open source document");
        let transcoder = NativeTranscoder::new();

        let mut video_plan = EditPlan::video(&document).unwrap();
        video_plan.trim = TrimRange::new(
            Duration::from_millis(500),
            Duration::from_millis(1_500),
            document.duration(),
        )
        .unwrap();
        video_plan.quality = crate::Quality::High;
        video_plan.resolution = crate::ResolutionCap::Half;
        video_plan.audio.channels = ChannelBehavior::StereoToMono;
        video_plan.audio.volume = 0.5;
        let video_path = directory.path.join("edited.mp4");
        let video_output = finished_output(
            transcoder
                .start(&document, &video_plan, video_path.clone())
                .unwrap(),
        );
        video_output.require_native().unwrap();
        assert_eq!(
            std::fs::metadata(&video_path).unwrap().len(),
            video_output.bytes_written
        );
        let edited = NativeMediaSource::open(
            Recording::native(&video_path, 1.0, "native test output").unwrap(),
        )
        .expect("inspect edited video");
        assert_eq!(
            (edited.metadata().width, edited.metadata().height),
            (48, 32)
        );
        assert_eq!(edited.metadata().audio_channels, 1);
        assert!(
            edited
                .inspection()
                .duration
                .abs_diff(Duration::from_secs(1))
                <= Duration::from_millis(100)
        );
        assert_mono_volume_was_applied(&edited);

        let mut muted_plan = EditPlan::video(&document).unwrap();
        muted_plan.trim = TrimRange::new(
            Duration::ZERO,
            Duration::from_millis(500),
            document.duration(),
        )
        .unwrap();
        muted_plan.quality = crate::Quality::Low;
        muted_plan.audio.mute = true;
        let muted_path = directory.path.join("muted.mp4");
        finished_output(
            transcoder
                .start(&document, &muted_plan, muted_path.clone())
                .unwrap(),
        );
        let muted = NativeMediaSource::open(
            Recording::native(&muted_path, 0.5, "native test output").unwrap(),
        )
        .expect("inspect muted video");
        assert_eq!(muted.metadata().audio_channels, 0);

        let mut gif_plan = EditPlan::gif(&document).unwrap();
        gif_plan.trim = TrimRange::new(
            Duration::from_millis(25),
            Duration::from_millis(525),
            document.duration(),
        )
        .unwrap();
        gif_plan.resolution = crate::ResolutionCap::Half;
        gif_plan.quality = crate::Quality::Low;
        let gif_path = directory.path.join("edited.gif");
        let gif_output = finished_output(
            transcoder
                .start(&document, &gif_plan, gif_path.clone())
                .unwrap(),
        );
        gif_output.require_native().unwrap();
        let gif = GifDecoder::new(std::io::BufReader::new(
            std::fs::File::open(&gif_path).unwrap(),
        ))
        .unwrap();
        let frames = gif.into_frames().collect_frames().unwrap();
        assert!(!frames.is_empty());
        assert_eq!(frames[0].buffer().dimensions(), (48, 32));
        let gif_duration_ms: u64 = frames
            .iter()
            .map(|frame| {
                let (numerator, denominator) = frame.delay().numer_denom_ms();
                u64::from(numerator) / u64::from(denominator)
            })
            .sum();
        assert_eq!(gif_duration_ms, 500);

        let retained = retain_partial(
            &gif_path,
            ArtifactKind::Gif,
            Duration::from_millis(500),
            "injected terminal failure",
        )
        .unwrap()
        .expect("playable bytes mandate a partial artifact");
        assert!(retained.is_partial());
        assert!(retained.provenance.is_native());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_gif_cancellation_retains_the_frames_already_written() {
        let directory = TestDirectory::new("cancel");
        let recording =
            write_native_silent_fixture(&directory.path.join("source.mp4"), (192, 128), 30, 180);
        let document = VideoDocument::open_native(recording).unwrap();
        let mut plan = EditPlan::gif(&document).unwrap();
        plan.quality = crate::Quality::High;
        let output_path = directory.path.join("cancelled.gif");
        let mut job = NativeTranscoder::new()
            .start(&document, &plan, output_path.clone())
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut cancellation_requested = false;
        loop {
            if let Some(event) = job.poll() {
                match event {
                    TranscodeEvent::Progress(_) if !cancellation_requested => {
                        job.cancel().expect("cancel active GIF export");
                        cancellation_requested = true;
                    }
                    TranscodeEvent::Progress(_) => {}
                    TranscodeEvent::Cancelled(Some(partial)) => {
                        assert!(cancellation_requested);
                        assert!(partial.is_partial());
                        assert!(partial.provenance.is_native());
                        assert_eq!(partial.path, output_path);
                        assert!(scrozz_export::inspect_gif_file(&partial.path).is_ok());
                        break;
                    }
                    TranscodeEvent::Cancelled(None) => {
                        panic!(
                            "frames were written before cancellation but no partial was retained"
                        )
                    }
                    TranscodeEvent::Finished(_) => {
                        panic!("large GIF export finished before cancellation")
                    }
                    TranscodeEvent::Failed(failure) => {
                        panic!("cancelled GIF export failed: {}", failure.error)
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "GIF cancellation timed out with status {:?}",
                job.status()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(job.status(), TranscodeStatus::Cancelled);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_video_failure_retains_a_decodable_partial() {
        let directory = TestDirectory::new("video-partial");
        let output_path = directory.path.join("partial.mp4");
        let cancelled = AtomicBool::new(false);
        let mut writer =
            platform::VideoWriter::new(&output_path, (96, 64), 10.0, crate::Quality::Low, 0)
                .unwrap();
        for index in 0..3_u64 {
            writer
                .append_video(
                    &DecodedVideoFrame {
                        timestamp: Duration::from_millis(index * 100),
                        duration: Duration::from_millis(100),
                        image: RgbaImage {
                            width: 96,
                            height: 64,
                            data: [index as u8 * 40, 80, 160, 255].repeat(96 * 64),
                        },
                    },
                    Duration::ZERO,
                    &cancelled,
                )
                .unwrap();
        }
        let WorkerTerminal::Failed(failure) = finish_failed_video(
            Error::Codec("injected post-frame failure".to_owned()),
            writer,
            &output_path,
        ) else {
            panic!("expected structured failure");
        };
        let partial = failure
            .partial
            .expect("decodable native bytes must be retained");
        assert!(partial.is_partial());
        assert!(partial.provenance.is_native());
        assert_eq!(partial.path, output_path);
        NativeMediaSource::open(
            Recording::native_partial(
                &partial.path,
                0.3,
                NativeTranscoder::name(),
                "injected post-frame failure",
            )
            .unwrap(),
        )
        .expect("retained video partial decodes");
    }

    #[cfg(target_os = "macos")]
    fn finished_output(mut job: Box<dyn TranscodeJob>) -> TranscodeOutput {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut previous = 0.0;
        loop {
            if let Some(event) = job.poll() {
                match event {
                    TranscodeEvent::Progress(progress) => {
                        assert!((previous..=1.0).contains(&progress));
                        previous = progress;
                    }
                    TranscodeEvent::Finished(output) => {
                        assert_eq!(previous, 1.0);
                        return output;
                    }
                    TranscodeEvent::Failed(failure) => {
                        panic!("native transcode failed: {}", failure.error)
                    }
                    TranscodeEvent::Cancelled(partial) => {
                        panic!("native transcode was cancelled: {partial:?}")
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "native transcode timed out with status {:?}",
                job.status()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[cfg(target_os = "macos")]
    fn assert_mono_volume_was_applied(source: &NativeMediaSource) {
        let range = TrimRange::full(source.inspection().duration).unwrap();
        let mut decoder = source.decoder(range).unwrap();
        let mut peak = 0.0_f32;
        for _ in 0..128 {
            match decoder.next_sample().unwrap() {
                Some(DecodedMediaSample::Audio(chunk)) => {
                    assert_eq!(chunk.channels, 1);
                    peak = chunk
                        .samples
                        .iter()
                        .fold(peak, |current, sample| current.max(sample.abs()));
                }
                Some(DecodedMediaSample::Video(_)) => {}
                None => break,
            }
        }
        assert!(
            (0.025..0.15).contains(&peak),
            "expected attenuated mono audio, got peak {peak}"
        );
    }

    #[cfg(target_os = "macos")]
    fn write_native_fixture(path: &Path) -> Recording {
        let cancelled = AtomicBool::new(false);
        let mut writer =
            platform::VideoWriter::new(path, (96, 64), 10.0, crate::Quality::Balanced, 2)
                .expect("open native fixture writer");
        for index in 0..20_u64 {
            let timestamp = Duration::from_millis(index * 100);
            let mut pixels = Vec::with_capacity(96 * 64 * 4);
            for pixel in 0..96 * 64 {
                let value = (pixel as u8).wrapping_add(index as u8 * 7);
                pixels.extend_from_slice(&[
                    value,
                    value.wrapping_add(47),
                    value.wrapping_add(93),
                    255,
                ]);
            }
            writer
                .append_video(
                    &DecodedVideoFrame {
                        timestamp,
                        duration: Duration::from_millis(100),
                        image: RgbaImage {
                            width: 96,
                            height: 64,
                            data: pixels,
                        },
                    },
                    Duration::ZERO,
                    &cancelled,
                )
                .expect("write fixture video");

            let mut samples = Vec::with_capacity(4_800 * 2);
            for sample in 0..4_800 {
                let phase = ((index * 4_800 + sample) as f32 * 440.0 * std::f32::consts::TAU
                    / 48_000.0)
                    .sin();
                samples.extend_from_slice(&[phase * 0.5, phase * -0.25]);
            }
            writer
                .append_audio(
                    &DecodedAudioChunk {
                        timestamp,
                        duration: Duration::from_millis(100),
                        sample_rate: 48_000,
                        channels: 2,
                        samples,
                    },
                    Duration::ZERO,
                    2,
                    1.0,
                    &cancelled,
                )
                .expect("write fixture audio");
        }
        writer
            .finish(Duration::from_secs(2))
            .expect("finish native fixture");
        Recording::native(path, 2.0, "native AVFoundation test fixture").unwrap()
    }

    #[cfg(target_os = "macos")]
    fn write_native_silent_fixture(
        path: &Path,
        dimensions: (u32, u32),
        fps: u32,
        frame_count: u64,
    ) -> Recording {
        let cancelled = AtomicBool::new(false);
        let mut writer = platform::VideoWriter::new(
            path,
            dimensions,
            f64::from(fps),
            crate::Quality::Balanced,
            0,
        )
        .expect("open native fixture writer");
        let frame_duration = Duration::from_secs_f64(1.0 / f64::from(fps));
        for index in 0..frame_count {
            let timestamp = frame_duration.saturating_mul(u32::try_from(index).unwrap());
            let mut pixels = Vec::with_capacity((dimensions.0 * dimensions.1 * 4) as usize);
            for pixel in 0..dimensions.0 * dimensions.1 {
                let value = (pixel as u8).wrapping_add((index as u8).wrapping_mul(11));
                pixels.extend_from_slice(&[
                    value,
                    value.wrapping_add(71),
                    value.wrapping_add(149),
                    255,
                ]);
            }
            writer
                .append_video(
                    &DecodedVideoFrame {
                        timestamp,
                        duration: frame_duration,
                        image: RgbaImage {
                            width: dimensions.0,
                            height: dimensions.1,
                            data: pixels,
                        },
                    },
                    Duration::ZERO,
                    &cancelled,
                )
                .expect("write fixture video");
        }
        let duration = frame_duration.saturating_mul(u32::try_from(frame_count).unwrap());
        writer.finish(duration).expect("finish native fixture");
        Recording::native(
            path,
            duration.as_secs_f64(),
            "native AVFoundation test fixture",
        )
        .unwrap()
    }

    #[cfg(target_os = "macos")]
    struct TestDirectory {
        path: PathBuf,
    }

    #[cfg(target_os = "macos")]
    impl TestDirectory {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "scrozz-media-{label}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("create media test directory");
            Self { path }
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.path).expect("remove media test directory");
        }
    }
}
