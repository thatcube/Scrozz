//! Pause-aware media timestamps.

use std::time::Duration;

use scrozz_core::{Error, Result};

/// Monotonic recording timeline driven by externally supplied clock readings.
#[derive(Debug, Clone)]
pub struct RecordingTimeline {
    started_at: Duration,
    last_clock: Duration,
    paused_at: Option<Duration>,
    paused_total: Duration,
}

impl RecordingTimeline {
    /// Begins a timeline at an arbitrary monotonic clock reading.
    #[must_use]
    pub const fn new(started_at: Duration) -> Self {
        Self {
            started_at,
            last_clock: started_at,
            paused_at: None,
            paused_total: Duration::ZERO,
        }
    }

    /// Pauses media time.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-monotonic clock or a repeated pause.
    pub fn pause(&mut self, now: Duration) -> Result<()> {
        self.advance_clock(now)?;
        if self.paused_at.replace(now).is_some() {
            return Err(Error::InvalidRequest(
                "recording timeline is already paused".into(),
            ));
        }
        Ok(())
    }

    /// Resumes media time.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-monotonic clock or when not paused.
    pub fn resume(&mut self, now: Duration) -> Result<()> {
        self.advance_clock(now)?;
        let paused_at = self
            .paused_at
            .take()
            .ok_or_else(|| Error::InvalidRequest("recording timeline is not paused".into()))?;
        self.paused_total = self
            .paused_total
            .saturating_add(now.saturating_sub(paused_at));
        Ok(())
    }

    /// Returns media time with all paused intervals removed.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-monotonic clock.
    pub fn media_time(&mut self, now: Duration) -> Result<Duration> {
        self.advance_clock(now)?;
        let clock = self.paused_at.unwrap_or(now);
        Ok(clock
            .saturating_sub(self.started_at)
            .saturating_sub(self.paused_total))
    }

    fn advance_clock(&mut self, now: Duration) -> Result<()> {
        if now < self.last_clock {
            return Err(Error::InvalidRequest(
                "recording clock moved backwards".into(),
            ));
        }
        self.last_clock = now;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::RecordingTimeline;

    #[test]
    fn paused_time_does_not_enter_media_timestamps() {
        let mut timeline = RecordingTimeline::new(Duration::from_secs(10));
        assert_eq!(
            timeline.media_time(Duration::from_secs(12)).unwrap(),
            Duration::from_secs(2)
        );
        timeline.pause(Duration::from_secs(13)).unwrap();
        assert_eq!(
            timeline.media_time(Duration::from_secs(20)).unwrap(),
            Duration::from_secs(3)
        );
        timeline.resume(Duration::from_secs(23)).unwrap();
        assert_eq!(
            timeline.media_time(Duration::from_secs(25)).unwrap(),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn rejects_invalid_transitions_and_non_monotonic_clocks() {
        let mut timeline = RecordingTimeline::new(Duration::from_secs(1));
        assert!(timeline.resume(Duration::from_secs(2)).is_err());
        assert!(timeline.media_time(Duration::ZERO).is_err());
    }
}
