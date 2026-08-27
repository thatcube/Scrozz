//! Transcoder contracts and deterministic synthetic jobs.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use scrozz_core::{Error, Result};

use crate::edit::{EditPlan, VideoDocument};

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
    /// Cancellation completed.
    Cancelled,
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
            return Some(TranscodeEvent::Cancelled);
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

    use crate::{
        Recording,
        edit::{EditPlan, SourceMetadata, VideoDocument},
    };

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
        assert!(matches!(job.poll(), Some(TranscodeEvent::Cancelled)));
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
    fn source_plan_is_revalidated_at_start() {
        let (document, mut plan) = fixture();
        plan.trim.end = document.duration() + Duration::from_secs(1);
        let transcoder = MockTranscoder::success(vec![], 1).unwrap();
        let result = transcoder.start(&document, &plan, PathBuf::from("out.mp4"));
        assert!(result.is_err());
    }
}
