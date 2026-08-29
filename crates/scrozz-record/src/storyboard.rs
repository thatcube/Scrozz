//! Bounded, incremental filmstrip and waveform sampling for the video editor.
//!
//! This worker owns an independent native decoder so timeline sampling never
//! seeks or stalls the authoritative playback clock. It retains a fixed number
//! of small RGBA frames and one normalized audio peak per slot.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use scrozz_core::{Error, Result};

use crate::{
    Recording,
    edit::{SourceMetadata, TrimRange, VideoDocument},
    media::{DecodedMediaSample, DecodedVideoFrame, NativeMediaSource},
};

/// Maximum decoded images retained by one editor filmstrip.
pub const STORYBOARD_SLOTS: usize = 12;
/// Maximum long edge of a retained filmstrip image.
pub const MAX_STORYBOARD_EDGE: u32 = 192;
/// Maximum decoded samples inspected for one timeline slot.
const MAX_SAMPLES_PER_SLOT: usize = 128;
const MIN_SAMPLE_WINDOW: Duration = Duration::from_millis(250);
const MAX_SAMPLE_WINDOW: Duration = Duration::from_secs(2);
static NEXT_STORYBOARD_STREAM: AtomicU64 = AtomicU64::new(1);

/// One incrementally decoded filmstrip image.
#[derive(Debug, Clone)]
pub struct StoryboardFrame {
    /// Requested source time represented by this slot.
    pub timestamp: Duration,
    /// First decoded video frame at or after the requested time.
    pub frame: Arc<DecodedVideoFrame>,
}

/// Cloneable timeline-analysis state crossing the recording/UI seam.
#[derive(Debug, Clone)]
pub struct StoryboardSnapshot {
    /// Process-local identity used to separate texture caches.
    pub stream_id: u64,
    /// Fixed source times sampled across the full recording.
    pub timestamps: [Duration; STORYBOARD_SLOTS],
    /// Decoded images, populated from left to right.
    pub frames: [Option<StoryboardFrame>; STORYBOARD_SLOTS],
    /// Normalized audio peaks, or `None` for a source with no audio track.
    pub waveform: Option<[Option<f32>; STORYBOARD_SLOTS]>,
    /// Whether every requested slot has settled.
    pub complete: bool,
    /// Explicit sampling failure. Already decoded slots remain usable.
    pub error: Option<String>,
}

impl Default for StoryboardSnapshot {
    fn default() -> Self {
        Self {
            stream_id: 0,
            timestamps: [Duration::ZERO; STORYBOARD_SLOTS],
            frames: std::array::from_fn(|_| None),
            waveform: None,
            complete: false,
            error: None,
        }
    }
}

/// Independent native timeline sampler with deterministic worker ownership.
pub struct NativeStoryboard {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl NativeStoryboard {
    /// Starts incremental source sampling on a named worker.
    ///
    /// # Errors
    ///
    /// Returns an error when native decoding is unavailable, source bounds are
    /// invalid, or the worker cannot be created.
    pub fn start(document: &VideoDocument) -> Result<Self> {
        let capabilities = crate::media::native_media_capabilities();
        if !capabilities.source_decode {
            return Err(Error::Unsupported {
                what: "recording timeline thumbnails".to_owned(),
                why: capabilities
                    .unavailable_reason
                    .unwrap_or("this platform has no native decoder")
                    .to_owned(),
            });
        }
        let duration = document.duration();
        if duration.is_zero() {
            return Err(Error::InvalidRequest(
                "a zero-duration recording has no timeline to sample".to_owned(),
            ));
        }
        let timestamps = sample_timestamps(duration);
        let snapshot = StoryboardSnapshot {
            stream_id: NEXT_STORYBOARD_STREAM.fetch_add(1, Ordering::Relaxed),
            timestamps,
            frames: std::array::from_fn(|_| None),
            waveform: (document.metadata().audio_channels > 0)
                .then(|| std::array::from_fn(|_| None)),
            complete: false,
            error: None,
        };
        let shared = Arc::new(Shared {
            cancelled: AtomicBool::new(false),
            snapshot: Mutex::new(snapshot),
        });
        let worker_shared = Arc::clone(&shared);
        let recording = document.recording().clone();
        let metadata = document.metadata();
        let worker = std::thread::Builder::new()
            .name("scrozz-recording-storyboard".to_owned())
            .spawn(move || run_worker(recording, metadata, duration, worker_shared))
            .map_err(|error| {
                Error::Platform(format!(
                    "could not start recording timeline sampler: {error}"
                ))
            })?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    /// Returns the latest bounded timeline snapshot without waiting for decode.
    #[must_use]
    pub fn snapshot(&self) -> StoryboardSnapshot {
        self.shared
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Cancels sampling and waits for its decoder worker to release the source.
    ///
    /// # Errors
    ///
    /// Returns an explicit platform error if the worker panicked.
    pub fn shutdown(&mut self) -> Result<()> {
        self.shared.cancelled.store(true, Ordering::Release);
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker.join().map_err(|_| {
            Error::Platform("the recording timeline sampler panicked during shutdown".to_owned())
        })
    }
}

impl Drop for NativeStoryboard {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Shared {
    cancelled: AtomicBool,
    snapshot: Mutex<StoryboardSnapshot>,
}

fn run_worker(
    recording: Recording,
    expected_metadata: SourceMetadata,
    duration: Duration,
    shared: Arc<Shared>,
) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        sample_storyboard(recording, expected_metadata, duration, &shared)
    }))
    .unwrap_or_else(|_| {
        Err(Error::Platform(
            "the recording timeline sampler panicked".to_owned(),
        ))
    });
    let mut snapshot = shared
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match result {
        Ok(completed) => snapshot.complete = completed,
        Err(error) => {
            snapshot.error = Some(error.to_string());
            snapshot.complete = true;
        }
    }
}

fn sample_storyboard(
    recording: Recording,
    expected_metadata: SourceMetadata,
    duration: Duration,
    shared: &Shared,
) -> Result<bool> {
    let source = NativeMediaSource::open(recording)?;
    if source.metadata() != expected_metadata || source.inspection().duration != duration {
        return Err(Error::Codec(
            "recording metadata changed while timeline sampling started".to_owned(),
        ));
    }
    let dimensions = storyboard_dimensions(expected_metadata.width, expected_metadata.height);
    let window = sample_window(expected_metadata.fps);
    let timestamps = sample_timestamps(duration);
    for (index, timestamp) in timestamps.into_iter().enumerate() {
        let terminal = index + 1 == STORYBOARD_SLOTS;
        if shared.cancelled.load(Ordering::Acquire) {
            return Ok(false);
        }
        let (start, end) = sample_range(timestamp, duration, window)?;
        let range = TrimRange::new(start, end, duration)?;
        let mut decoder = source.decoder_with_dimensions(range, dimensions)?;
        let mut frame = None;
        let mut peak = None;
        for _ in 0..MAX_SAMPLES_PER_SLOT {
            if shared.cancelled.load(Ordering::Acquire) {
                decoder.cancel();
                return Ok(false);
            }
            match decoder.next_sample()? {
                Some(DecodedMediaSample::Video(decoded)) if terminal || frame.is_none() => {
                    frame = Some(Arc::new(decoded));
                }
                Some(DecodedMediaSample::Audio(chunk)) => {
                    let chunk_peak = chunk
                        .samples
                        .iter()
                        .copied()
                        .map(f32::abs)
                        .fold(0.0_f32, f32::max)
                        .clamp(0.0, 1.0);
                    peak = Some(peak.map_or(chunk_peak, |current: f32| current.max(chunk_peak)));
                }
                Some(DecodedMediaSample::Video(_)) => {}
                None => break,
            }
            if !terminal
                && frame.is_some()
                && (expected_metadata.audio_channels == 0 || peak.is_some())
            {
                break;
            }
        }
        decoder.cancel();
        let frame = frame.ok_or_else(|| {
            Error::Codec(format!(
                "timeline sampler decoded no video near {:.3} seconds",
                timestamp.as_secs_f64()
            ))
        })?;
        let mut snapshot = shared
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.frames[index] = Some(StoryboardFrame { timestamp, frame });
        if let Some(waveform) = &mut snapshot.waveform {
            waveform[index] = peak;
        }
    }
    Ok(true)
}

fn sample_range(
    timestamp: Duration,
    duration: Duration,
    window: Duration,
) -> Result<(Duration, Duration)> {
    let mut start = timestamp.min(duration);
    let mut end = start.saturating_add(window).min(duration);
    if start >= end {
        start = duration.saturating_sub(window.min(duration));
        end = duration;
    }
    if start >= end {
        return Err(Error::InvalidRequest(
            "recording is too short to sample a timeline frame".to_owned(),
        ));
    }
    Ok((start, end))
}

#[allow(clippy::cast_precision_loss)]
fn sample_timestamps(duration: Duration) -> [Duration; STORYBOARD_SLOTS] {
    let denominator = (STORYBOARD_SLOTS - 1) as f64;
    std::array::from_fn(|index| {
        Duration::try_from_secs_f64(duration.as_secs_f64() * index as f64 / denominator)
            .unwrap_or(duration)
    })
}

fn sample_window(fps: f64) -> Duration {
    Duration::try_from_secs_f64((4.0 / fps.max(1.0)).clamp(
        MIN_SAMPLE_WINDOW.as_secs_f64(),
        MAX_SAMPLE_WINDOW.as_secs_f64(),
    ))
    .unwrap_or(MIN_SAMPLE_WINDOW)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn storyboard_dimensions(width: u32, height: u32) -> (u32, u32) {
    let edge = width.max(height);
    if edge <= MAX_STORYBOARD_EDGE {
        return (width.max(1), height.max(1));
    }
    let scale = f64::from(MAX_STORYBOARD_EDGE) / f64::from(edge);
    (
        (f64::from(width) * scale).round().max(1.0) as u32,
        (f64::from(height) * scale).round().max(1.0) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_grid_covers_the_source_with_a_fixed_bound() {
        let duration = Duration::from_secs(110);
        let timestamps = sample_timestamps(duration);
        assert_eq!(timestamps.len(), STORYBOARD_SLOTS);
        assert_eq!(timestamps[0], Duration::ZERO);
        assert_eq!(timestamps[STORYBOARD_SLOTS - 1], duration);
        assert!(timestamps.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn storyboard_dimensions_preserve_aspect_and_bound_memory() {
        assert_eq!(storyboard_dimensions(1920, 1080), (192, 108));
        assert_eq!(storyboard_dimensions(1080, 1920), (108, 192));
        assert_eq!(storyboard_dimensions(96, 64), (96, 64));
        let bytes = STORYBOARD_SLOTS * 192 * 192 * 4;
        assert!(bytes < 2 * 1024 * 1024);
    }

    #[test]
    fn terminal_sample_uses_a_valid_backward_window() {
        let duration = Duration::from_secs(10);
        let (start, end) = sample_range(duration, duration, Duration::from_millis(250)).unwrap();
        assert_eq!(start, Duration::from_millis(9_750));
        assert_eq!(end, duration);
    }
}
