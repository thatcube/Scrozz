//! Recording-session state transitions.

use scrozz_core::{Error, Result};

/// Externally visible recording state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderState {
    /// Resources are being acquired.
    Starting,
    /// Frames are being accepted.
    Recording,
    /// Capture is alive but media time is stopped.
    Paused,
    /// Encoders are draining and the final fragment is being flushed.
    Stopping,
    /// The recording closed normally.
    Finished,
    /// The recording retained a partial result after an error.
    Failed,
}

/// User/session command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderCommand {
    /// Resource acquisition completed.
    Started,
    /// Pause media.
    Pause,
    /// Resume media.
    Resume,
    /// Finalise.
    Stop,
    /// Finalisation completed.
    Finish,
    /// Preserve partial output and end.
    Fail,
}

/// Small deterministic state machine shared by all platform sessions.
#[derive(Debug, Clone)]
pub struct RecordingStateMachine {
    state: RecorderState,
}

impl RecordingStateMachine {
    /// Creates a session in resource-acquisition state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: RecorderState::Starting,
        }
    }

    /// Current state.
    #[must_use]
    pub const fn state(&self) -> RecorderState {
        self.state
    }

    /// Applies one command.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when the transition is not legal.
    pub fn apply(&mut self, command: RecorderCommand) -> Result<RecorderState> {
        let next = match (self.state, command) {
            (RecorderState::Starting, RecorderCommand::Started) => RecorderState::Recording,
            (RecorderState::Recording, RecorderCommand::Pause) => RecorderState::Paused,
            (RecorderState::Paused, RecorderCommand::Resume) => RecorderState::Recording,
            (RecorderState::Recording | RecorderState::Paused, RecorderCommand::Stop) => {
                RecorderState::Stopping
            }
            (RecorderState::Stopping, RecorderCommand::Finish) => RecorderState::Finished,
            (
                RecorderState::Starting
                | RecorderState::Recording
                | RecorderState::Paused
                | RecorderState::Stopping,
                RecorderCommand::Fail,
            ) => RecorderState::Failed,
            _ => {
                return Err(Error::InvalidRequest(format!(
                    "cannot apply {command:?} while recorder is {:?}",
                    self.state
                )));
            }
        };
        self.state = next;
        Ok(next)
    }
}

impl Default for RecordingStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{RecorderCommand, RecorderState, RecordingStateMachine};

    #[test]
    fn allows_the_full_lifecycle() {
        let mut state = RecordingStateMachine::new();
        for (command, expected) in [
            (RecorderCommand::Started, RecorderState::Recording),
            (RecorderCommand::Pause, RecorderState::Paused),
            (RecorderCommand::Resume, RecorderState::Recording),
            (RecorderCommand::Stop, RecorderState::Stopping),
            (RecorderCommand::Finish, RecorderState::Finished),
        ] {
            assert_eq!(state.apply(command).unwrap(), expected);
        }
    }

    #[test]
    fn refuses_double_pause_and_stop_after_finish() {
        let mut state = RecordingStateMachine::new();
        state.apply(RecorderCommand::Started).unwrap();
        state.apply(RecorderCommand::Pause).unwrap();
        assert!(state.apply(RecorderCommand::Pause).is_err());
        state.apply(RecorderCommand::Stop).unwrap();
        state.apply(RecorderCommand::Finish).unwrap();
        assert!(state.apply(RecorderCommand::Stop).is_err());
    }
}
