//! Capture-cadence decisions independent of a wall clock.

use std::time::Duration;

/// Decision produced for an arriving capture frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingDecision {
    /// The frame arrived before the next constant-rate slot.
    Drop,
    /// Encode this frame.
    Emit {
        /// Constant-rate frame number.
        frame_index: u64,
        /// Number of empty slots since the previous accepted frame. A recorder
        /// can fill them by repeating the prior encoded image.
        repeat_previous: u64,
    },
}

/// Maps irregular capture arrivals onto a constant frame rate.
#[derive(Debug, Clone)]
pub struct FramePacer {
    fps: u32,
    next_frame: u64,
}

impl FramePacer {
    /// Creates a pacer for a validated non-zero frame rate.
    #[must_use]
    pub const fn new(fps: u32) -> Self {
        debug_assert!(fps > 0);
        Self { fps, next_frame: 0 }
    }

    /// Observes a pause-adjusted timestamp.
    #[must_use]
    pub fn observe(&mut self, timestamp: Duration) -> PacingDecision {
        let nanos = timestamp.as_nanos();
        let frame = nanos
            .saturating_mul(u128::from(self.fps))
            .saturating_add(500_000_000)
            / 1_000_000_000;
        let frame = u64::try_from(frame).unwrap_or(u64::MAX);
        if frame < self.next_frame {
            return PacingDecision::Drop;
        }
        let repeat_previous = frame.saturating_sub(self.next_frame);
        self.next_frame = frame.saturating_add(1);
        PacingDecision::Emit {
            frame_index: frame,
            repeat_previous,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{FramePacer, PacingDecision};

    #[test]
    fn drops_early_frames_and_reports_gaps() {
        let mut pacer = FramePacer::new(30);
        assert_eq!(
            pacer.observe(Duration::ZERO),
            PacingDecision::Emit {
                frame_index: 0,
                repeat_previous: 0
            }
        );
        assert_eq!(
            pacer.observe(Duration::from_millis(10)),
            PacingDecision::Drop
        );
        assert_eq!(
            pacer.observe(Duration::from_millis(70)),
            PacingDecision::Emit {
                frame_index: 2,
                repeat_previous: 1
            }
        );
    }

    #[test]
    fn long_timelines_do_not_accumulate_floating_point_drift() {
        let mut pacer = FramePacer::new(60);
        assert_eq!(
            pacer.observe(Duration::from_secs(60 * 60 * 24)),
            PacingDecision::Emit {
                frame_index: 5_184_000,
                repeat_previous: 5_184_000
            }
        );
    }
}
