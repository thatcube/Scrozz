//! Pure one-shot terminal-result caching for the Windows adapter.

use std::{
    io::ErrorKind,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use scrozz_core::{Error, Result};

use crate::{Recording, RecordingProvenance, RecordingState, SessionEvent};

/// Internal lifecycle state shared between the worker and its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Running,
    Paused,
    Ended,
}

/// Lock-free state published by the worker, including asynchronous endings.
pub struct SharedSessionState(AtomicU8);

impl SharedSessionState {
    /// Starts in the active recording state.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU8::new(SessionState::Running as u8))
    }

    /// Current internal state.
    #[must_use]
    pub fn get(&self) -> SessionState {
        match self.0.load(Ordering::Acquire) {
            value if value == SessionState::Paused as u8 => SessionState::Paused,
            value if value == SessionState::Ended as u8 => SessionState::Ended,
            _ => SessionState::Running,
        }
    }

    /// Current shared contract state.
    #[must_use]
    pub fn recording_state(&self) -> RecordingState {
        match self.get() {
            SessionState::Running => RecordingState::Recording,
            SessionState::Paused => RecordingState::Paused,
            SessionState::Ended => RecordingState::Stopped,
        }
    }

    /// Publishes a worker transition.
    pub fn set(&self, state: SessionState) {
        self.0.store(state as u8, Ordering::Release);
    }
}

impl Default for SharedSessionState {
    fn default() -> Self {
        Self::new()
    }
}

/// A recording proven to have come from this native adapter.
#[derive(Debug, Clone)]
pub struct NativeRecording(Recording);

impl NativeRecording {
    /// Validates the private worker-to-owner result boundary.
    pub fn new(recording: Recording, expected_engine: &str) -> Result<Self> {
        recording.validate()?;
        match &recording.provenance {
            RecordingProvenance::Native {
                engine,
                target: Some(_),
            } if engine == expected_engine => Ok(Self(recording)),
            RecordingProvenance::Native { engine, target } => Err(Error::Platform(format!(
                "Windows recording worker returned invalid native provenance \
                 (engine {engine:?}, target present: {})",
                target.is_some()
            ))),
            RecordingProvenance::Synthetic { generator } => Err(Error::Platform(format!(
                "Windows recording worker rejected synthetic output from {generator:?}"
            ))),
        }
    }

    fn as_recording(&self) -> &Recording {
        &self.0
    }

    fn into_recording(self) -> Recording {
        self.0
    }
}

/// Cloneable representation of the non-cloneable core error enum.
#[derive(Debug, Clone)]
pub enum CachedError {
    PermissionDenied { capability: String, remedy: String },
    Unsupported { what: String, why: String },
    TargetGone(String),
    InvalidRequest(String),
    Codec(String),
    Storage(String),
    Cancelled,
    Io { kind: ErrorKind, message: String },
    Platform(String),
}

impl CachedError {
    fn new(error: Error) -> Self {
        match error {
            Error::PermissionDenied { capability, remedy } => {
                Self::PermissionDenied { capability, remedy }
            }
            Error::Unsupported { what, why } => Self::Unsupported { what, why },
            Error::TargetGone(message) => Self::TargetGone(message),
            Error::InvalidRequest(message) => Self::InvalidRequest(message),
            Error::Codec(message) => Self::Codec(message),
            Error::Storage(message) => Self::Storage(message),
            Error::Cancelled => Self::Cancelled,
            Error::Io(error) => Self::Io {
                kind: error.kind(),
                message: error.to_string(),
            },
            Error::Platform(message) => Self::Platform(message),
            other => Self::Platform(other.to_string()),
        }
    }

    fn to_error(&self) -> Error {
        match self {
            Self::PermissionDenied { capability, remedy } => Error::PermissionDenied {
                capability: capability.clone(),
                remedy: remedy.clone(),
            },
            Self::Unsupported { what, why } => Error::Unsupported {
                what: what.clone(),
                why: why.clone(),
            },
            Self::TargetGone(message) => Error::TargetGone(message.clone()),
            Self::InvalidRequest(message) => Error::InvalidRequest(message.clone()),
            Self::Codec(message) => Error::Codec(message.clone()),
            Self::Storage(message) => Error::Storage(message.clone()),
            Self::Cancelled => Error::Cancelled,
            Self::Io { kind, message } => Error::Io(std::io::Error::new(*kind, message.clone())),
            Self::Platform(message) => Error::Platform(message.clone()),
        }
    }
}

/// Frozen terminal semantics retained after the one-shot event is emitted.
#[derive(Debug, Clone)]
pub enum TerminalOutcome {
    Finished(NativeRecording),
    Failed(CachedError),
}

impl TerminalOutcome {
    fn from_result(result: Result<NativeRecording>) -> Self {
        match result {
            Ok(recording) => Self::Finished(recording),
            Err(error) => Self::Failed(CachedError::new(error)),
        }
    }

    fn event(&self) -> SessionEvent {
        match self {
            Self::Finished(recording) => SessionEvent::Finished(recording.as_recording().clone()),
            Self::Failed(error) => SessionEvent::Failed(Arc::new(error.to_error())),
        }
    }

    fn into_result(self) -> Result<Recording> {
        match self {
            Self::Finished(recording) => Ok(recording.into_recording()),
            Self::Failed(error) => Err(error.to_error()),
        }
    }

    /// Finished output retained by an abandoned owner.
    pub fn recording(&self) -> Option<&Recording> {
        match self {
            Self::Finished(recording) => Some(recording.as_recording()),
            Self::Failed(_) => None,
        }
    }

    /// Failure retained by an abandoned owner.
    pub fn error(&self) -> Option<Error> {
        match self {
            Self::Finished(_) => None,
            Self::Failed(error) => Some(error.to_error()),
        }
    }
}

/// Stores the first terminal outcome and emits its event at most once.
#[derive(Debug, Default)]
pub struct TerminalCache {
    outcome: Option<TerminalOutcome>,
    emitted: bool,
}

impl TerminalCache {
    /// Freezes the first result; later terminal messages cannot mutate it.
    pub fn cache(&mut self, result: Result<NativeRecording>) {
        if self.outcome.is_none() {
            self.outcome = Some(TerminalOutcome::from_result(result));
        }
    }

    /// Freezes a local owner-side failure when no worker result can arrive.
    pub fn cache_error(&mut self, error: Error) {
        self.cache(Err(error));
    }

    /// Whether a terminal result has been frozen.
    #[must_use]
    pub const fn is_some(&self) -> bool {
        self.outcome.is_some()
    }

    /// Emits a terminal event exactly once.
    pub fn emit(&mut self) -> Option<SessionEvent> {
        if self.emitted {
            return None;
        }
        let event = self.outcome.as_ref()?.event();
        self.emitted = true;
        Some(event)
    }

    /// Consumes the exact cached semantics returned by the worker.
    pub fn take_result(&mut self) -> Option<Result<Recording>> {
        self.outcome.take().map(TerminalOutcome::into_result)
    }

    /// Cached terminal details for abandoned-owner diagnostics.
    #[must_use]
    pub fn outcome(&self) -> Option<&TerminalOutcome> {
        self.outcome.as_ref()
    }
}
