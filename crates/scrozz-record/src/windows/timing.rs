//! Pure timestamp, pacing, and backpressure policy.

/// Media Foundation and WASAPI both express stream time in 100 ns units.
pub const HNS_PER_SECOND: i64 = 10_000_000;

/// Converts a QueryPerformanceCounter value to 100 ns units without overflow.
#[must_use]
pub fn qpc_to_hns(counter: i64, frequency: i64) -> Option<i64> {
    if counter < 0 || frequency <= 0 {
        return None;
    }
    let scaled = i128::from(counter) * i128::from(HNS_PER_SECOND);
    i64::try_from(scaled / i128::from(frequency)).ok()
}

/// Maps absolute QPC-based timestamps onto a pause-free recording timeline.
#[derive(Debug, Default, Clone)]
pub struct Timeline {
    origin: Option<i64>,
    paused_total: i64,
    pause_started: Option<i64>,
    last_mapped: i64,
}

impl Timeline {
    /// Starts the timeline at the first accepted video frame.
    pub fn start(&mut self, raw_hns: i64) {
        if self.origin.is_none() {
            self.origin = Some(raw_hns);
            self.last_mapped = 0;
        }
    }

    /// Marks a pause. Repeated pauses are idempotent.
    pub fn pause(&mut self, raw_hns: i64) {
        if self.pause_started.is_none() {
            self.pause_started = Some(raw_hns);
        }
    }

    /// Ends a pause and removes it from all later stream timestamps.
    pub fn resume(&mut self, raw_hns: i64) {
        let Some(started) = self.pause_started.take() else {
            return;
        };
        if let Some(origin) = self.origin {
            let effective_start = started.max(origin);
            self.paused_total = self
                .paused_total
                .saturating_add(raw_hns.saturating_sub(effective_start).max(0));
        }
    }

    /// Maps a timestamp, or returns `None` while paused or before video starts.
    pub fn map(&mut self, raw_hns: i64) -> Option<i64> {
        let mapped = self.project(raw_hns)?;
        self.last_mapped = self.last_mapped.max(mapped);
        Some(mapped)
    }

    /// Projects a timestamp without claiming that media was emitted through it.
    #[must_use]
    pub fn project(&self, raw_hns: i64) -> Option<i64> {
        if self.pause_started.is_some() {
            return None;
        }
        let origin = self.origin?;
        Some(
            raw_hns
                .saturating_sub(origin)
                .saturating_sub(self.paused_total)
                .max(0),
        )
    }

    /// The latest emitted stream time.
    #[must_use]
    pub const fn duration_hns(&self) -> i64 {
        self.last_mapped
    }

    /// Whether capture is currently paused.
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        self.pause_started.is_some()
    }
}

/// One frame assigned to an exact rational output slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduledFrame {
    /// Phase-locked sample timestamp.
    pub timestamp_hns: i64,
    /// Exact duration to the next rational slot boundary.
    pub duration_hns: i64,
}

/// Drops compositor frames that exceed the requested recording rate.
#[derive(Debug, Clone)]
pub struct FramePacer {
    fps: u32,
    last_slot: Option<u64>,
}

impl FramePacer {
    /// Creates a pacer for a validated frame rate.
    #[must_use]
    pub fn new(fps: u32) -> Self {
        Self {
            fps: fps.max(1),
            last_slot: None,
        }
    }

    /// Assigns the first input in each output slot to that slot's exact phase.
    pub fn schedule(&mut self, stream_hns: i64) -> Option<ScheduledFrame> {
        if stream_hns < 0 {
            return None;
        }
        // WGC/QPC conversion truncates to one 100 ns tick. Give that single
        // tick back so a nominal 30 Hz source does not miss every exact 30 fps
        // slot by the conversion remainder.
        let phase = i128::from(stream_hns.saturating_add(1)) * i128::from(self.fps);
        let slot = u64::try_from(phase / i128::from(HNS_PER_SECOND)).ok()?;
        if self.last_slot.is_some_and(|last| slot <= last) {
            return None;
        }
        self.last_slot = Some(slot);

        let timestamp_hns = slot_hns(slot, self.fps);
        let end_hns = slot_hns(slot.saturating_add(1), self.fps);
        Some(ScheduledFrame {
            timestamp_hns,
            duration_hns: end_hns.saturating_sub(timestamp_hns).max(1),
        })
    }
}

fn slot_hns(slot: u64, fps: u32) -> i64 {
    ((i128::from(slot) * i128::from(HNS_PER_SECOND)) / i128::from(fps))
        .try_into()
        .unwrap_or(i64::MAX)
}

/// Bounded queues drop the newest frame rather than invalidating older timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backpressure {
    /// There is room for the packet.
    Enqueue,
    /// Preserve queued packets and drop the newly arrived packet.
    DropNewest,
}

/// Chooses the queue action for a fixed capacity.
#[must_use]
pub const fn backpressure(len: usize, capacity: usize) -> Backpressure {
    if capacity != 0 && len < capacity {
        Backpressure::Enqueue
    } else {
        Backpressure::DropNewest
    }
}

/// Chooses how far audio may be silence-filled without outrunning late WASAPI
/// packets. Finalisation deliberately fills the entire remaining media tail.
#[must_use]
pub fn audio_drain_limit(
    media_end_hns: i64,
    latest_video_hns: i64,
    qpc_watermark_hns: i64,
    settle_hns: i64,
    finalising: bool,
) -> i64 {
    if finalising {
        media_end_hns.max(0)
    } else {
        let settled_capture = latest_video_hns
            .max(qpc_watermark_hns)
            .saturating_sub(settle_hns)
            .max(0);
        media_end_hns.min(settled_capture).max(0)
    }
}

/// Converts stream time to an audio-frame index.
#[must_use]
pub fn hns_to_audio_frame(stream_hns: i64, sample_rate: u32) -> u64 {
    if stream_hns <= 0 {
        return 0;
    }
    ((i128::from(stream_hns) * i128::from(sample_rate)) / i128::from(HNS_PER_SECOND))
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Converts audio frames to stream time.
#[must_use]
pub fn audio_frames_to_hns(frames: u64, sample_rate: u32) -> i64 {
    if sample_rate == 0 {
        return 0;
    }
    ((i128::from(frames) * i128::from(HNS_PER_SECOND)) / i128::from(sample_rate))
        .try_into()
        .unwrap_or(i64::MAX)
}
