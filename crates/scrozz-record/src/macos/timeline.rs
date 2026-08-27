//! Recording and sample timelines.
//!
//! ScreenCaptureKit keeps advancing while a session is paused. These models
//! remove that discarded source-time gap rather than encoding a frozen or
//! black interval.

use std::time::Duration;

use crate::RecordingState;

#[derive(Debug)]
pub(crate) struct SessionTimeline {
    state: RecordingState,
    started: Duration,
    paused_at: Option<Duration>,
    paused_total: Duration,
    stopped_elapsed: Option<Duration>,
}

impl SessionTimeline {
    pub(crate) fn new(now: Duration) -> Self {
        Self {
            state: RecordingState::Recording,
            started: now,
            paused_at: None,
            paused_total: Duration::ZERO,
            stopped_elapsed: None,
        }
    }

    pub(crate) const fn state(&self) -> RecordingState {
        self.state
    }

    pub(crate) fn elapsed(&self, now: Duration) -> Duration {
        if let Some(elapsed) = self.stopped_elapsed {
            return elapsed;
        }

        let end = self.paused_at.unwrap_or(now);
        end.saturating_sub(self.started)
            .saturating_sub(self.paused_total)
    }

    pub(crate) fn pause(&mut self, now: Duration) -> Result<(), &'static str> {
        match self.state {
            RecordingState::Recording => {
                self.state = RecordingState::Paused;
                self.paused_at = Some(now);
                Ok(())
            }
            RecordingState::Paused => Ok(()),
            RecordingState::Stopped => Err("recording has already stopped"),
        }
    }

    pub(crate) fn resume(&mut self, now: Duration) -> Result<(), &'static str> {
        match self.state {
            RecordingState::Paused => {
                if let Some(paused_at) = self.paused_at.take() {
                    self.paused_total += now.saturating_sub(paused_at);
                }
                self.state = RecordingState::Recording;
                Ok(())
            }
            RecordingState::Recording => Ok(()),
            RecordingState::Stopped => Err("recording has already stopped"),
        }
    }

    pub(crate) fn stop(&mut self, now: Duration) -> Duration {
        let elapsed = self.elapsed(now);
        self.stopped_elapsed = Some(elapsed);
        self.paused_at = None;
        self.state = RecordingState::Stopped;
        elapsed
    }
}

#[derive(Debug, Default)]
pub(crate) struct SampleTimeline {
    last_source_end_ns: Option<i64>,
    last_output_end_ns: Option<i64>,
    remove_gap_on_next_sample: bool,
    removed_ns: i64,
}

impl SampleTimeline {
    pub(crate) fn pause(&mut self) {
        self.remove_gap_on_next_sample = true;
    }

    pub(crate) fn map(&mut self, source_ns: i64, duration_ns: i64) -> i64 {
        if self.remove_gap_on_next_sample {
            if let Some(last_end) = self.last_source_end_ns {
                self.removed_ns = self
                    .removed_ns
                    .saturating_add(source_ns.saturating_sub(last_end).max(0));
            }
            self.remove_gap_on_next_sample = false;
        }

        let output_ns = source_ns
            .saturating_sub(self.removed_ns)
            .max(self.last_output_end_ns.unwrap_or(i64::MIN));
        let duration_ns = duration_ns.max(0);
        self.last_source_end_ns = Some(source_ns.saturating_add(duration_ns));
        self.last_output_end_ns = Some(output_ns.saturating_add(duration_ns));
        output_ns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_and_elapsed_time_exclude_pauses() {
        let mut timeline = SessionTimeline::new(Duration::from_secs(10));
        assert_eq!(
            timeline.elapsed(Duration::from_secs(13)),
            Duration::from_secs(3)
        );

        timeline.pause(Duration::from_secs(13)).unwrap();
        assert_eq!(timeline.state(), RecordingState::Paused);
        assert_eq!(
            timeline.elapsed(Duration::from_secs(20)),
            Duration::from_secs(3)
        );

        timeline.resume(Duration::from_secs(20)).unwrap();
        assert_eq!(timeline.state(), RecordingState::Recording);
        assert_eq!(
            timeline.elapsed(Duration::from_secs(22)),
            Duration::from_secs(5)
        );

        assert_eq!(
            timeline.stop(Duration::from_secs(23)),
            Duration::from_secs(6)
        );
        assert_eq!(timeline.state(), RecordingState::Stopped);
        assert_eq!(
            timeline.elapsed(Duration::from_secs(99)),
            Duration::from_secs(6)
        );
    }

    #[test]
    fn sample_timestamps_remove_the_discarded_pause_gap() {
        let mut timeline = SampleTimeline::default();
        assert_eq!(timeline.map(1_000, 10), 1_000);
        assert_eq!(timeline.map(1_010, 10), 1_010);

        timeline.pause();
        assert_eq!(timeline.map(5_000, 10), 1_020);
        assert_eq!(timeline.map(5_010, 10), 1_030);
    }

    #[test]
    fn sample_timestamps_never_move_backwards() {
        let mut timeline = SampleTimeline::default();
        assert_eq!(timeline.map(100, 20), 100);
        assert_eq!(timeline.map(105, 20), 120);
    }
}
