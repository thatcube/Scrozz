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

    pub(crate) fn resume(&mut self, now: Duration) -> Result<Duration, &'static str> {
        match self.state {
            RecordingState::Paused => {
                let paused = self
                    .paused_at
                    .take()
                    .map_or(Duration::ZERO, |paused_at| now.saturating_sub(paused_at));
                self.paused_total += paused;
                self.state = RecordingState::Recording;
                Ok(paused)
            }
            RecordingState::Recording => Ok(Duration::ZERO),
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
    last_output_end_ns: Option<i64>,
    removed_ns: i64,
}

impl SampleTimeline {
    pub(crate) fn remove_pause(&mut self, paused_ns: i64, session_started: bool) {
        if session_started {
            self.removed_ns = self.removed_ns.saturating_add(paused_ns.max(0));
        }
    }

    pub(crate) fn map(
        &mut self,
        source_ns: i64,
        duration_ns: i64,
        minimum_output_ns: Option<i64>,
    ) -> i64 {
        let output_ns = source_ns
            .saturating_sub(self.removed_ns)
            .max(self.last_output_end_ns.unwrap_or(i64::MIN))
            .max(minimum_output_ns.unwrap_or(i64::MIN));
        let duration_ns = duration_ns.max(0);
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
        assert_eq!(timeline.map(1_000, 10, None), 1_000);
        assert_eq!(timeline.map(1_010, 10, None), 1_010);

        timeline.remove_pause(3_980, true);
        assert_eq!(timeline.map(5_000, 10, None), 1_020);
        assert_eq!(timeline.map(5_010, 10, None), 1_030);
    }

    #[test]
    fn sample_timestamps_keep_static_time_before_a_pause() {
        let mut timeline = SampleTimeline::default();
        assert_eq!(timeline.map(1_000, 10, None), 1_000);

        timeline.remove_pause(100, true);

        assert_eq!(timeline.map(5_000, 10, None), 4_900);
    }

    #[test]
    fn a_pause_before_the_first_sample_does_not_shift_the_session_origin() {
        let mut timeline = SampleTimeline::default();
        timeline.remove_pause(5_000, false);
        assert_eq!(timeline.map(10_000, 10, None), 10_000);
    }

    #[test]
    fn a_track_starting_after_pause_uses_the_shared_session_timeline() {
        let mut timeline = SampleTimeline::default();
        timeline.remove_pause(5_000, true);
        assert_eq!(timeline.map(10_000, 10, None), 5_000);
    }

    #[test]
    fn a_retained_pause_frame_cannot_precede_the_shared_session() {
        let mut timeline = SampleTimeline::default();
        timeline.remove_pause(2_000, true);
        assert_eq!(timeline.map(100_200, 10, Some(100_000)), 100_000);
    }

    #[test]
    fn sample_timestamps_never_move_backwards() {
        let mut timeline = SampleTimeline::default();
        assert_eq!(timeline.map(100, 20, None), 100);
        assert_eq!(timeline.map(105, 20, None), 120);
    }
}
