//! Fragmented MP4 writing through AVAssetWriter and VideoToolbox.

use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{io::Read, io::Seek};
use std::{os::unix::ffi::OsStrExt as _, os::unix::fs::PermissionsExt as _};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterStatus, AVFileTypeMPEG4, AVMediaTypeAudio,
    AVMediaTypeVideo, AVURLAsset,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags};
use objc2_foundation::NSURL;
use scrozz_core::{Error, Result};

use crate::Salvageability;

use super::mix::LiveMixer;
use super::pcm;
use super::plan::RecordingPlan;
use super::settings;
use super::timeline::SampleTimeline;

const FINISH_TIMEOUT: Duration = Duration::from_secs(30);
const FINISH_CANCELLATION_GRACE: Duration = Duration::from_secs(5);
const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;
static RECOVERY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioTrack {
    System,
    Microphone,
}

pub(crate) struct Writer {
    asset: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    audio: Option<Retained<AVAssetWriterInput>>,
    video_timeline: SampleTimeline,
    audio_timeline: SampleTimeline,
    video_frame_duration_ns: i64,
    audio_mixer: LiveMixer,
    path: PathBuf,
    destination: PathBuf,
    working_directory: Option<PathBuf>,
    frames: u64,
    dropped_frames: u64,
    has_audio: bool,
    media_end_ns: i64,
    session_origin_ns: Option<i64>,
    paused: bool,
    finished: bool,
}

// SAFETY: the writer and all of its inputs are accessed only while holding the
// owning `Mutex<Writer>`. AVAssetWriter explicitly permits status/error reads
// across threads, and sample appends/finalization are serialized here.
unsafe impl Send for Writer {}

pub(crate) struct WriterSummary {
    pub(crate) path: PathBuf,
    pub(crate) frames: u64,
    pub(crate) has_audio: bool,
    pub(crate) duration: Duration,
    pub(crate) partial: Option<String>,
    pub(crate) salvageability: Option<Salvageability>,
}

impl Writer {
    pub(crate) fn new(
        requested_path: Option<&Path>,
        plan: &RecordingPlan,
        fps: u32,
        system_audio: bool,
        microphone: bool,
    ) -> Result<Self> {
        let destination = destination(requested_path)?;
        if destination.exists() {
            return Err(Error::InvalidRequest(format!(
                "recording destination already exists: {}",
                destination.display()
            )));
        }

        if let Some(parent) = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let working = WorkingOutput::create(&destination)?;
        let path = working.path.clone();

        // SAFETY: immutable weak-linked AVFoundation constant.
        let file_type = unsafe { AVFileTypeMPEG4 }.ok_or_else(|| Error::Unsupported {
            what: "MP4 recording".to_owned(),
            why: "AVFoundation did not expose the MPEG-4 container type".to_owned(),
        })?;
        let url = file_url(&path)?;
        // SAFETY: URL and file type are valid, retained Objective-C objects.
        let asset = unsafe {
            AVAssetWriter::assetWriterWithURL_fileType_error(&url, file_type).map_err(
                |failure| Error::Storage(super::error::describe(&failure, "opening MP4")),
            )?
        };

        // SAFETY: immutable weak-linked AVFoundation constant.
        let video_type = unsafe { AVMediaTypeVideo }.ok_or_else(|| Error::Unsupported {
            what: "video recording".to_owned(),
            why: "AVFoundation did not expose the video media type".to_owned(),
        })?;
        let video_settings = settings::video(plan, fps)?;
        // SAFETY: settings use AVFoundation's documented video keys and values.
        if !unsafe { asset.canApplyOutputSettings_forMediaType(Some(&*video_settings), video_type) }
        {
            return Err(Error::Unsupported {
                what: format!("{:?} hardware video encoding", plan.codec),
                why: "VideoToolbox rejected the requested dimensions or hardware-only settings"
                    .to_owned(),
            });
        }
        // SAFETY: settings were validated above and are fully specified.
        let video = unsafe {
            AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                video_type,
                Some(&*video_settings),
            )
        };
        // SAFETY: plain input properties and a guarded add operation.
        unsafe {
            video.setExpectsMediaDataInRealTime(true);
            if !asset.canAddInput(&video) {
                return Err(Error::Codec(
                    "AVAssetWriter refused the video input".to_owned(),
                ));
            }
            asset.addInput(&video);
        }

        let audio = if system_audio || microphone {
            Some(add_audio_input(&asset)?)
        } else {
            None
        };

        // SAFETY: all writer configuration is complete and no samples have been
        // submitted. A short fragment interval makes an interrupted MP4 salvageable.
        unsafe {
            asset.setMovieFragmentInterval(CMTime::new(plan.fragment_interval_seconds, 1));
            asset.setShouldOptimizeForNetworkUse(true);
            if !asset.startWriting() {
                return Err(writer_failure(
                    &asset,
                    "starting the hardware video encoder",
                ));
            }
        }

        let (working_directory, path) = working.disarm();
        Ok(Self {
            asset,
            video,
            audio,
            video_timeline: SampleTimeline::default(),
            audio_timeline: SampleTimeline::default(),
            video_frame_duration_ns: NANOSECONDS_PER_SECOND / i64::from(fps),
            audio_mixer: LiveMixer::new(system_audio, microphone),
            path,
            destination,
            working_directory: Some(working_directory),
            frames: 0,
            dropped_frames: 0,
            has_audio: false,
            media_end_ns: 0,
            session_origin_ns: None,
            paused: false,
            finished: false,
        })
    }

    pub(crate) fn pause(&mut self) -> Result<()> {
        self.flush_audio()?;
        self.paused = true;
        Ok(())
    }

    pub(crate) fn resume(&mut self, paused: Duration) {
        let paused_ns = i64::try_from(paused.as_nanos()).unwrap_or(i64::MAX);
        let session_started = self.session_origin_ns.is_some();
        self.video_timeline.remove_pause(paused_ns, session_started);
        self.audio_timeline.remove_pause(paused_ns, session_started);
        self.paused = false;
    }

    pub(crate) fn video_ready(&self) -> bool {
        !self.paused && unsafe { self.video.isReadyForMoreMediaData() }
    }

    pub(crate) fn append_video(&mut self, sample: &CMSampleBuffer) -> Result<bool> {
        if self.paused {
            return Ok(false);
        }
        // SAFETY: readiness is a thread-safe property read.
        if !unsafe { self.video.isReadyForMoreMediaData() } {
            self.dropped_frames = self.dropped_frames.saturating_add(1);
            return Ok(false);
        }
        let (source_pts, source_ns) = sample_time(sample)?;
        self.ensure_session(source_pts, source_ns);
        let (sample, end_ns) = rebase(
            sample,
            &mut self.video_timeline,
            source_ns,
            self.video_frame_duration_ns,
            self.session_origin_ns.unwrap_or(source_ns),
        )?;
        // SAFETY: the input belongs to this active writer and the sample carries
        // a video image buffer from ScreenCaptureKit.
        if !unsafe { self.video.appendSampleBuffer(&sample) } {
            return Err(writer_failure(&self.asset, "encoding a video frame"));
        }
        self.frames = self.frames.saturating_add(1);
        self.media_end_ns = self
            .media_end_ns
            .max(end_ns.saturating_sub(self.session_origin_ns.unwrap_or(end_ns)));
        Ok(true)
    }

    pub(crate) fn append_audio(
        &mut self,
        track: AudioTrack,
        sample: &CMSampleBuffer,
    ) -> Result<()> {
        if self.paused {
            return Ok(());
        }
        let Some(input) = self.audio.clone() else {
            return Ok(());
        };
        // SAFETY: readiness is a thread-safe property read. Dropping live audio
        // under pressure is preferable to blocking the ScreenCaptureKit queue.
        if !unsafe { input.isReadyForMoreMediaData() } {
            return Ok(());
        }
        let (source_pts, source_ns) = sample_time(sample)?;
        self.ensure_session(source_pts, source_ns);
        let chunk = pcm::decode(sample)?;
        let mixed = match track {
            AudioTrack::System => self.audio_mixer.push_system(chunk),
            AudioTrack::Microphone => self.audio_mixer.push_microphone(chunk),
        };
        if let Some(mixed) = mixed {
            self.append_mixed_audio(&mixed)?;
        }
        Ok(())
    }

    fn append_mixed_audio(&mut self, mixed: &super::mix::PcmChunk) -> Result<()> {
        let Some(input) = self.audio.clone() else {
            return Ok(());
        };
        // SAFETY: readiness is a thread-safe property read. Dropping live audio
        // under pressure is preferable to blocking the ScreenCaptureKit queue.
        if !unsafe { input.isReadyForMoreMediaData() } {
            return Ok(());
        }
        let sample = pcm::encode(mixed)?;
        let (_, source_ns) = sample_time(&sample)?;
        let (sample, end_ns) = rebase(
            &sample,
            &mut self.audio_timeline,
            source_ns,
            0,
            self.session_origin_ns.unwrap_or(source_ns),
        )?;
        // SAFETY: the input is an audio input owned by this active writer.
        if !unsafe { input.appendSampleBuffer(&sample) } {
            return Err(writer_failure(&self.asset, "encoding an audio sample"));
        }
        self.has_audio = true;
        self.media_end_ns = self
            .media_end_ns
            .max(end_ns.saturating_sub(self.session_origin_ns.unwrap_or(end_ns)));
        Ok(())
    }

    fn flush_audio(&mut self) -> Result<()> {
        if let Some(mixed) = self.audio_mixer.flush() {
            self.append_mixed_audio(&mixed)?;
        }
        Ok(())
    }

    fn ensure_session(&mut self, source_pts: CMTime, source_ns: i64) {
        if self.session_origin_ns.is_none() {
            // SAFETY: startWriting succeeded and no sample-writing session has
            // begun. Using the first source timestamp preserves untouched
            // sample buffers until a pause actually requires rebasing.
            unsafe {
                self.asset.startSessionAtSourceTime(source_pts);
            }
            self.session_origin_ns = Some(source_ns);
        }
    }

    pub(crate) fn finish(
        &mut self,
        interrupted: Option<String>,
        session_duration: Duration,
    ) -> Result<WriterSummary> {
        if self.finished {
            return Err(Error::InvalidRequest(
                "recording writer was already finalised".to_owned(),
            ));
        }
        self.finished = true;

        let mut partial = interrupted;
        if let Err(failure) = self.flush_audio() {
            append_reason(&mut partial, &failure.to_string());
        }

        // SAFETY: writer status is a documented thread-safe property read.
        let writer_is_active = unsafe { self.asset.status() == AVAssetWriterStatus::Writing };
        if let Some(origin_ns) = self.session_origin_ns.filter(|_| writer_is_active) {
            let elapsed_ns = i64::try_from(session_duration.as_nanos()).unwrap_or(i64::MAX);
            self.media_end_ns = self.media_end_ns.max(elapsed_ns);
            let end_ns = origin_ns.saturating_add(self.media_end_ns).max(origin_ns);
            // SAFETY: the writer session started at the numeric source origin,
            // and no more samples can arrive after capture shutdown.
            unsafe {
                self.asset
                    .endSessionAtSourceTime(CMTime::new(end_ns, NANOSECONDS_PER_SECOND as i32));
            }
        }

        // SAFETY: capture has stopped, so no further appends can race these calls.
        unsafe {
            self.video.markAsFinished();
            if let Some(input) = &self.audio {
                input.markAsFinished();
            }
        }

        let completed = Arc::new((Mutex::new(false), Condvar::new()));
        let completion = {
            let completed = Arc::clone(&completed);
            RcBlock::new(move || {
                let (done, ready) = &*completed;
                *done.lock().unwrap_or_else(PoisonError::into_inner) = true;
                ready.notify_all();
            })
        };
        // SAFETY: the block owns its synchronization state and remains valid
        // until AVAssetWriter invokes it.
        unsafe {
            self.asset.finishWritingWithCompletionHandler(&completion);
        }

        let (done, ready) = &*completed;
        let (done, _) = ready
            .wait_timeout_while(
                done.lock().unwrap_or_else(PoisonError::into_inner),
                FINISH_TIMEOUT,
                |done| !*done,
            )
            .unwrap_or_else(PoisonError::into_inner);
        let timed_out = !*done;
        drop(done);

        let mut timeout_recovery = Vec::new();
        let mut cancelled_after_timeout = false;
        if timed_out {
            append_reason(
                &mut partial,
                "AVAssetWriter did not finish within 30 seconds",
            );
            let mut candidates = vec![self.path.clone()];
            let mut can_cancel = true;
            match sidecar_paths(&self.path) {
                Ok(paths) => candidates.extend(paths),
                Err(failure) => {
                    can_cancel = false;
                    append_reason(
                        &mut partial,
                        &format!(
                            "could not inspect AVAssetWriter files before cancellation: {failure}"
                        ),
                    );
                }
            }
            for candidate in candidates {
                match preserve_before_cancellation(&candidate) {
                    Ok(Some(recovery)) => timeout_recovery.push(recovery),
                    Ok(None) => {}
                    Err(failure) => {
                        can_cancel = false;
                        append_reason(
                            &mut partial,
                            &format!(
                                "could not preserve {} before cancelling AVAssetWriter: {failure}",
                                candidate.display()
                            ),
                        );
                    }
                }
            }
            if can_cancel {
                // SAFETY: all non-empty candidates have an independent durable
                // path, and AVAssetWriter blocks until no further writes remain.
                unsafe {
                    self.asset.cancelWriting();
                }
                cancelled_after_timeout = true;
            } else {
                append_reason(
                    &mut partial,
                    "some output could not be preserved before AVAssetWriter cancellation",
                );
                let (done, _) = completed
                    .1
                    .wait_timeout_while(
                        completed.0.lock().unwrap_or_else(PoisonError::into_inner),
                        FINISH_CANCELLATION_GRACE,
                        |done| !*done,
                    )
                    .unwrap_or_else(PoisonError::into_inner);
                let still_running = !*done;
                drop(done);
                if still_running {
                    // SAFETY: finalization has exceeded both deadlines. Any
                    // preservable bytes were copied above; cancellation is
                    // preferable to hanging stop or application shutdown.
                    unsafe {
                        self.asset.cancelWriting();
                    }
                    cancelled_after_timeout = true;
                }
            }
        }
        if !cancelled_after_timeout {
            // SAFETY: status and error are thread-safe after completion.
            let status = unsafe { self.asset.status() };
            if status != AVAssetWriterStatus::Completed {
                let reason = unsafe {
                    self.asset.error().map_or_else(
                        || format!("AVAssetWriter ended with status {}", status.0),
                        |failure| super::error::describe(&failure, "finalising MP4"),
                    )
                };
                append_reason(&mut partial, &reason);
            }
        }

        if self.frames == 0 {
            append_reason(
                &mut partial,
                "recording ended before any video frame was encoded",
            );
        }

        let mut new_sidecars = match sidecar_paths(&self.path) {
            Ok(paths) => paths,
            Err(failure) => {
                tracing::warn!(
                    path = %self.path.display(),
                    "could not enumerate AVAssetWriter sidecars during finalization: {failure}"
                );
                if partial.is_some() {
                    append_reason(
                        &mut partial,
                        &format!("could not inspect AVAssetWriter recovery files: {failure}"),
                    );
                }
                Vec::new()
            }
        };
        new_sidecars.extend(timeout_recovery);
        new_sidecars.sort();
        new_sidecars.dedup();
        let Some((candidate, retained)) = best_retained_output(&self.path, &new_sidecars) else {
            let failure = discard_unreportable(
                &self.path,
                &new_sidecars,
                Error::Storage(format!(
                    "recording failed before a reportable MP4 initialization was written: {}",
                    partial
                        .as_deref()
                        .unwrap_or("writer produced no playable video")
                )),
            );
            self.remove_working_directory();
            return Err(failure);
        };
        if partial.is_none() && retained != Salvageability::Playable {
            append_reason(
                &mut partial,
                "AVAssetWriter completed without a playable video fragment",
            );
        }
        let mut output_path = candidate.clone();
        if let Err(failure) = promote_no_replace(&candidate, &self.destination) {
            append_reason(&mut partial, &failure);
        } else {
            output_path = self.destination.clone();
        }
        if candidate != self.path && output_path != self.path {
            remove_file_warn(&self.path);
        }
        cleanup_sidecars(&new_sidecars, &output_path);
        if output_path == self.destination {
            self.remove_working_directory();
        } else {
            self.working_directory = None;
        }
        let salvageability = partial.as_ref().map(|_| retained);

        Ok(WriterSummary {
            path: output_path,
            frames: self.frames,
            has_audio: self.has_audio,
            duration: Duration::from_nanos(self.media_end_ns.max(0) as u64),
            partial,
            salvageability,
        })
    }

    pub(crate) fn discard(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        // SAFETY: the owning session stopped accepting callbacks before
        // abandoning the writer, and this call synchronously releases its
        // encoder state.
        unsafe {
            self.asset.cancelWriting();
        }

        let mut cleanup_failures = Vec::new();
        let mut candidates = vec![self.path.clone()];
        match sidecar_paths(&self.path) {
            Ok(sidecars) => candidates.extend(sidecars),
            Err(failure) => cleanup_failures.push(format!(
                "could not enumerate abandoned AVAssetWriter sidecars: {failure}"
            )),
        }
        candidates.sort();
        candidates.dedup();
        for candidate in candidates {
            if let Err(failure) = std::fs::remove_file(&candidate)
                && failure.kind() != std::io::ErrorKind::NotFound
            {
                cleanup_failures.push(format!("{}: {failure}", candidate.display()));
            }
        }
        self.remove_working_directory();
        if cleanup_failures.is_empty() {
            Ok(())
        } else {
            Err(Error::Storage(format!(
                "could not fully remove abandoned recording output: {}",
                cleanup_failures.join("; ")
            )))
        }
    }

    fn remove_working_directory(&mut self) {
        if let Some(directory) = self.working_directory.take() {
            cleanup_working_directory(&directory);
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if let Err(failure) = self.discard() {
            tracing::error!(
                path = %self.path.display(),
                "abandoned recording cleanup failed: {failure}"
            );
        }
    }
}

struct WorkingOutput {
    directory: PathBuf,
    path: PathBuf,
    armed: bool,
}

impl WorkingOutput {
    fn create(destination: &Path) -> Result<Self> {
        let parent = destination
            .parent()
            .filter(|value| !value.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        loop {
            let sequence = RECOVERY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let directory =
                parent.join(format!(".scrozz-writer-{}-{sequence}", std::process::id()));
            match std::fs::create_dir(&directory) {
                Ok(()) => {
                    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
                    return Ok(Self {
                        path: directory.join("recording.mp4"),
                        directory,
                        armed: true,
                    });
                }
                Err(failure) if failure.kind() == std::io::ErrorKind::AlreadyExists => {
                    continue;
                }
                Err(failure) => return Err(failure.into()),
            }
        }
    }

    fn disarm(mut self) -> (PathBuf, PathBuf) {
        self.armed = false;
        (self.directory.clone(), self.path.clone())
    }
}

impl Drop for WorkingOutput {
    fn drop(&mut self) {
        if self.armed {
            cleanup_working_directory(&self.directory);
        }
    }
}

fn file_url(path: &Path) -> Result<Retained<NSURL>> {
    NSURL::from_file_path(path).ok_or_else(|| {
        Error::InvalidRequest(format!(
            "recording destination cannot be represented as a macOS file URL: {}",
            path.display()
        ))
    })
}

fn add_audio_input(asset: &AVAssetWriter) -> Result<Retained<AVAssetWriterInput>> {
    // SAFETY: immutable weak-linked AVFoundation constant.
    let audio_type = unsafe { AVMediaTypeAudio }.ok_or_else(|| Error::Unsupported {
        what: "audio recording".to_owned(),
        why: "AVFoundation did not expose the audio media type".to_owned(),
    })?;
    let output_settings = settings::audio();
    // SAFETY: the settings fully specify AAC at 48 kHz stereo.
    let input = unsafe {
        AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
            audio_type,
            Some(&*output_settings),
        )
    };
    // SAFETY: plain property writes and guarded input addition before startWriting.
    unsafe {
        input.setExpectsMediaDataInRealTime(true);
        if !asset.canAddInput(&input) {
            return Err(Error::Codec(
                "AVAssetWriter refused an AAC audio input".to_owned(),
            ));
        }
        asset.addInput(&input);
    }
    Ok(input)
}

enum TimedSample<'a> {
    Original(&'a CMSampleBuffer),
    Rebased(CFRetained<CMSampleBuffer>),
}

impl std::ops::Deref for TimedSample<'_> {
    type Target = CMSampleBuffer;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Original(sample) => sample,
            Self::Rebased(sample) => sample,
        }
    }
}

fn sample_time(sample: &CMSampleBuffer) -> Result<(CMTime, i64)> {
    // SAFETY: immutable sample-buffer property read.
    let source_pts = unsafe { sample.presentation_time_stamp() };
    let source_ns = time_to_ns(source_pts).ok_or_else(|| {
        Error::Codec("ScreenCaptureKit delivered a sample without a numeric timestamp".to_owned())
    })?;
    Ok((source_pts, source_ns))
}

fn rebase<'a>(
    sample: &'a CMSampleBuffer,
    timeline: &mut SampleTimeline,
    source_ns: i64,
    fallback_duration_ns: i64,
    minimum_output_ns: i64,
) -> Result<(TimedSample<'a>, i64)> {
    let duration_ns = effective_duration_ns(
        unsafe { time_to_ns(sample.duration()) },
        fallback_duration_ns,
    );
    let output_ns = timeline.map(source_ns, duration_ns, Some(minimum_output_ns));
    let shift_ns = output_ns.saturating_sub(source_ns);
    if shift_ns == 0 {
        return Ok((
            TimedSample::Original(sample),
            output_ns.saturating_add(duration_ns),
        ));
    }

    // SAFETY: immutable sample-buffer property read.
    let source_pts = unsafe { sample.presentation_time_stamp() };

    let mut count = 0;
    // SAFETY: a null output array is the documented size-query operation.
    unsafe {
        let _ = sample.sample_timing_info_array(0, std::ptr::null_mut(), &mut count);
    }
    let mut timings = if count > 0 {
        vec![zero_timing(); count as usize]
    } else {
        vec![CMSampleTimingInfo {
            // SAFETY: valid construction of rational CoreMedia times.
            duration: unsafe { sample.duration() },
            presentationTimeStamp: source_pts,
            // SAFETY: immutable property read.
            decodeTimeStamp: unsafe { sample.decode_time_stamp() },
        }]
    };
    if count > 0 {
        // SAFETY: the vector is allocated for exactly the count requested by
        // CoreMedia and both output pointers remain valid for the call.
        let status =
            unsafe { sample.sample_timing_info_array(count, timings.as_mut_ptr(), &mut count) };
        if status != 0 {
            return Err(Error::Codec(format!(
                "reading sample timing failed with status {status}"
            )));
        }
    }

    for timing in &mut timings {
        if fallback_duration_ns > 0
            && time_to_ns(timing.duration).is_none_or(|duration| duration <= 0)
        {
            // SAFETY: nanoseconds use a valid positive timescale.
            timing.duration =
                unsafe { CMTime::new(fallback_duration_ns, NANOSECONDS_PER_SECOND as i32) };
        }
        timing.presentationTimeStamp = shift_time(timing.presentationTimeStamp, shift_ns);
        timing.decodeTimeStamp = shift_time(timing.decodeTimeStamp, shift_ns);
    }

    let mut output = std::ptr::null_mut();
    // SAFETY: timings points to initialized entries for the duration of the call,
    // and output is a valid non-null pointer to receive the created object.
    let status = unsafe {
        CMSampleBuffer::create_copy_with_new_timing(
            None,
            sample,
            timings.len() as isize,
            timings.as_ptr(),
            NonNull::new_unchecked(&mut output),
        )
    };
    if status != 0 {
        return Err(Error::Codec(format!(
            "rebasing sample timing failed with status {status}"
        )));
    }
    let output = NonNull::new(output)
        .ok_or_else(|| Error::Codec("CoreMedia returned no rebased sample buffer".to_owned()))?;
    // SAFETY: the Create rule transfers one owned retain to the caller.
    let output = unsafe { CFRetained::from_raw(output) };
    Ok((
        TimedSample::Rebased(output),
        output_ns.saturating_add(duration_ns),
    ))
}

fn effective_duration_ns(duration_ns: Option<i64>, fallback_duration_ns: i64) -> i64 {
    duration_ns
        .filter(|duration| *duration > 0)
        .unwrap_or(fallback_duration_ns.max(0))
}

fn time_to_ns(time: CMTime) -> Option<i64> {
    let flags = time.flags;
    let timescale = time.timescale;
    let value = time.value;
    if !flags.contains(CMTimeFlags::Valid)
        || flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
        || timescale <= 0
    {
        return None;
    }
    let value = i128::from(value) * i128::from(NANOSECONDS_PER_SECOND) / i128::from(timescale);
    Some(value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64)
}

fn shift_time(time: CMTime, shift_ns: i64) -> CMTime {
    time_to_ns(time).map_or(time, |value| {
        // SAFETY: one billion is a valid positive CMTime scale.
        unsafe {
            CMTime::new(
                value.saturating_add(shift_ns),
                NANOSECONDS_PER_SECOND as i32,
            )
        }
    })
}

fn zero_timing() -> CMSampleTimingInfo {
    // SAFETY: an all-zero CMTime has no Valid flag and is CoreMedia's invalid
    // sentinel representation; the fields are overwritten by CoreMedia.
    unsafe { std::mem::zeroed() }
}

fn writer_failure(asset: &AVAssetWriter, context: &str) -> Error {
    // SAFETY: status/error are documented thread-safe property reads.
    unsafe {
        asset.error().map_or_else(
            || Error::Codec(format!("{context}: AVAssetWriter rejected the operation")),
            |failure| Error::Codec(super::error::describe(&failure, context)),
        )
    }
}

fn destination(requested: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = requested {
        return Ok(path.to_owned());
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|failure| {
            Error::Platform(format!("system clock is before Unix epoch: {failure}"))
        })?
        .as_nanos();
    Ok(std::env::temp_dir().join(format!("scrozz-recording-{nonce}.mp4")))
}

pub(crate) fn sidecar_paths(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let Some(file_name) = path.file_name() else {
        return Ok(Vec::new());
    };
    let mut prefix = file_name.as_bytes().to_vec();
    prefix.extend_from_slice(b".sb-");
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        if entry.file_name().as_bytes().starts_with(&prefix) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

pub(crate) fn best_retained_output(
    path: &Path,
    sidecars: &[PathBuf],
) -> Option<(PathBuf, Salvageability)> {
    let classify = |candidate: &Path| match mp4_salvageability(candidate) {
        Ok(value) => value,
        Err(failure) => {
            tracing::warn!(
                path = %candidate.display(),
                "could not inspect retained AVAssetWriter output: {failure}"
            );
            None
        }
    };

    if classify(path) == Some(Salvageability::Playable) {
        return Some((path.to_owned(), Salvageability::Playable));
    }
    if let Some(candidate) = sidecars
        .iter()
        .find(|candidate| classify(candidate) == Some(Salvageability::Playable))
    {
        return Some((candidate.clone(), Salvageability::Playable));
    }
    if classify(path) == Some(Salvageability::InitialisationOnly) {
        return Some((path.to_owned(), Salvageability::InitialisationOnly));
    }
    sidecars.iter().find_map(|candidate| {
        (classify(candidate) == Some(Salvageability::InitialisationOnly))
            .then(|| (candidate.clone(), Salvageability::InitialisationOnly))
    })
}

fn promote_no_replace(source: &Path, destination: &Path) -> std::result::Result<(), String> {
    std::fs::hard_link(source, destination).map_err(|failure| {
        format!(
            "retained output remains at {} because it could not be promoted without replacing {}: {failure}",
            source.display(),
            destination.display()
        )
    })?;
    remove_file_warn(source);
    Ok(())
}

fn cleanup_sidecars(sidecars: &[PathBuf], retained: &Path) {
    for sidecar in sidecars {
        if sidecar == retained {
            continue;
        }
        if let Err(failure) = std::fs::remove_file(sidecar)
            && failure.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %sidecar.display(),
                "could not remove an AVAssetWriter sidecar: {failure}"
            );
        }
    }
}

fn remove_file_warn(path: &Path) {
    if let Err(failure) = std::fs::remove_file(path)
        && failure.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %path.display(),
            "could not remove superseded AVAssetWriter output: {failure}"
        );
    }
}

fn cleanup_working_directory(directory: &Path) {
    match std::fs::read_dir(directory) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => remove_file_warn(&entry.path()),
                    Err(failure) => tracing::warn!(
                        path = %directory.display(),
                        "could not inspect recording working directory: {failure}"
                    ),
                }
            }
        }
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => return,
        Err(failure) => {
            tracing::warn!(
                path = %directory.display(),
                "could not inspect recording working directory: {failure}"
            );
            return;
        }
    }
    if let Err(failure) = std::fs::remove_dir(directory)
        && failure.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %directory.display(),
            "could not remove recording working directory: {failure}"
        );
    }
}

pub(crate) fn preserve_before_cancellation(path: &Path) -> std::io::Result<Option<PathBuf>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(None),
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(failure) => return Err(failure),
    };
    if metadata.len() == 0 {
        return Ok(None);
    }

    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map_or_else(|| "recording".into(), |value| value.to_string_lossy());
    loop {
        let sequence = RECOVERY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let recovery = parent.join(format!(
            ".{file_name}.scrozz-recovery-{}-{sequence}",
            std::process::id()
        ));
        match std::fs::hard_link(path, &recovery) {
            Ok(()) => return Ok(Some(recovery)),
            Err(failure) if failure.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                match std::fs::rename(path, &recovery) {
                    Ok(()) => return Ok(Some(recovery)),
                    Err(failure) if failure.kind() == std::io::ErrorKind::AlreadyExists => {
                        continue;
                    }
                    Err(_) => {}
                }
                let mut source = std::fs::File::open(path)?;
                let mut destination = match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&recovery)
                {
                    Ok(destination) => destination,
                    Err(failure) if failure.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(failure) => return Err(failure),
                };
                if let Err(failure) = std::io::copy(&mut source, &mut destination)
                    .and_then(|_| destination.sync_all())
                {
                    let _ = std::fs::remove_file(&recovery);
                    return Err(failure);
                }
                return Ok(Some(recovery));
            }
        }
    }
}

#[derive(Default)]
struct Mp4Shape {
    has_file_type: bool,
    has_movie: bool,
    has_track: bool,
    has_media_box: bool,
    has_media: bool,
    has_samples: bool,
}

fn mp4_salvageability(path: &Path) -> std::io::Result<Option<Salvageability>> {
    let mut file = std::fs::File::open(path)?;
    let file_length = file.metadata()?.len();
    let mut shape = Mp4Shape::default();
    scan_mp4_boxes(&mut file, 0, file_length, false, false, &mut shape)?;
    if shape.has_movie
        && shape.has_track
        && shape.has_media
        && shape.has_samples
        && avfoundation_reports_playable_video(path)
    {
        Ok(Some(Salvageability::Playable))
    } else {
        Ok(
            (shape.has_movie && shape.has_track && (shape.has_file_type || shape.has_media_box))
                .then_some(Salvageability::InitialisationOnly),
        )
    }
}

fn avfoundation_reports_playable_video(path: &Path) -> bool {
    if path.extension().is_some_and(|extension| extension == "mp4") {
        return avfoundation_reports_playable_mp4(path);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    for _ in 0..100 {
        let sequence = RECOVERY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let probe = parent.join(format!(
            ".scrozz-avfoundation-probe-{}-{sequence}.mp4",
            std::process::id()
        ));
        match std::fs::hard_link(path, &probe) {
            Ok(()) => {
                let playable = avfoundation_reports_playable_mp4(&probe);
                remove_file_warn(&probe);
                return playable;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(hard_link_error) => {
                let mut source = match std::fs::File::open(path) {
                    Ok(source) => source,
                    Err(error) => {
                        tracing::warn!(path = %path.display(), %error, "could not open AVAssetWriter recovery for probing");
                        return false;
                    }
                };
                let mut destination = match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&probe)
                {
                    Ok(destination) => destination,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            %hard_link_error,
                            %error,
                            "could not copy an MP4-named AVAssetWriter recovery probe"
                        );
                        return false;
                    }
                };
                if let Err(error) = std::io::copy(&mut source, &mut destination)
                    .and_then(|_| destination.sync_all())
                {
                    remove_file_warn(&probe);
                    tracing::warn!(
                        path = %path.display(),
                        %hard_link_error,
                        %error,
                        "could not finish an MP4-named AVAssetWriter recovery probe"
                    );
                    return false;
                }
                let playable = avfoundation_reports_playable_mp4(&probe);
                remove_file_warn(&probe);
                return playable;
            }
        }
    }
    false
}

fn avfoundation_reports_playable_mp4(path: &Path) -> bool {
    let Some(url) = NSURL::from_file_path(path) else {
        return false;
    };
    // SAFETY: the URL names a local file and no options are needed for a
    // synchronous validation read.
    let asset = unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) };
    // SAFETY: immutable weak-linked AVFoundation constant and asset metadata
    // reads. The tracks call forces the relevant key to be loaded before the
    // playability decision.
    let Some(video_type) = (unsafe { AVMediaTypeVideo }) else {
        return false;
    };
    #[allow(deprecated)]
    let has_video = unsafe { !asset.tracksWithMediaType(video_type).is_empty() };
    let duration = unsafe { time_to_ns(asset.duration()) }.unwrap_or_default();
    has_video && duration > 0 && unsafe { asset.isPlayable() }
}

fn scan_mp4_boxes(
    file: &mut std::fs::File,
    mut offset: u64,
    end: u64,
    in_track: bool,
    in_fragment: bool,
    shape: &mut Mp4Shape,
) -> std::io::Result<()> {
    while offset.saturating_add(8) <= end {
        file.seek(std::io::SeekFrom::Start(offset))?;
        let mut header = [0_u8; 8];
        file.read_exact(&mut header)?;
        let short_size = u32::from_be_bytes(header[..4].try_into().expect("four bytes"));
        let kind: [u8; 4] = header[4..].try_into().expect("four bytes");
        let (box_size, header_size) = match short_size {
            0 => (end - offset, 8_u64),
            1 => {
                if offset.saturating_add(16) > end {
                    break;
                }
                let mut extended = [0_u8; 8];
                file.read_exact(&mut extended)?;
                (u64::from_be_bytes(extended), 16)
            }
            size => (u64::from(size), 8_u64),
        };
        let box_end = offset.saturating_add(box_size);
        if box_size < header_size || box_end > end {
            break;
        }
        let payload = offset + header_size;
        match &kind {
            b"ftyp" => shape.has_file_type = true,
            b"moov" => {
                shape.has_movie = true;
                scan_mp4_boxes(file, payload, box_end, false, false, shape)?;
            }
            b"trak" => {
                shape.has_track = true;
                scan_mp4_boxes(file, payload, box_end, true, false, shape)?;
            }
            b"mdia" | b"minf" | b"stbl" => {
                scan_mp4_boxes(file, payload, box_end, in_track, in_fragment, shape)?;
            }
            b"moof" => {
                scan_mp4_boxes(file, payload, box_end, false, true, shape)?;
            }
            b"traf" => {
                scan_mp4_boxes(file, payload, box_end, in_track, in_fragment, shape)?;
            }
            b"stsz" | b"stz2" if in_track && box_end.saturating_sub(payload) >= 12 => {
                shape.has_samples |= read_u32(file, payload + 8)? > 0;
            }
            b"trun" if in_fragment && box_end.saturating_sub(payload) >= 8 => {
                shape.has_samples |= read_u32(file, payload + 4)? > 0;
            }
            b"mdat" => {
                shape.has_media_box = true;
                shape.has_media |= box_end > payload;
            }
            _ => {}
        }
        if box_size == 0 {
            break;
        }
        offset = box_end;
    }
    Ok(())
}

fn read_u32(file: &mut std::fs::File, offset: u64) -> std::io::Result<u32> {
    file.seek(std::io::SeekFrom::Start(offset))?;
    let mut value = [0_u8; 4];
    file.read_exact(&mut value)?;
    Ok(u32::from_be_bytes(value))
}

fn discard_unreportable(path: &Path, sidecars: &[PathBuf], failure: Error) -> Error {
    let mut cleanup_failures = Vec::new();
    for candidate in std::iter::once(path).chain(sidecars.iter().map(PathBuf::as_path)) {
        if let Err(cleanup) = std::fs::remove_file(candidate)
            && cleanup.kind() != std::io::ErrorKind::NotFound
        {
            cleanup_failures.push(format!("{}: {cleanup}", candidate.display()));
        }
    }
    if cleanup_failures.is_empty() {
        failure
    } else {
        Error::Storage(format!(
            "{failure}; removing unreportable output also failed: {}",
            cleanup_failures.join("; ")
        ))
    }
}

fn append_reason(partial: &mut Option<String>, reason: &str) {
    if let Some(existing) = partial {
        existing.push_str("; ");
        existing.push_str(reason);
    } else {
        *partial = Some(reason.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    use super::*;

    #[test]
    fn recording_file_urls_preserve_non_utf8_path_bytes() {
        let path = PathBuf::from(OsString::from_vec(
            b"/tmp/scrozz-recording-\xFF.mp4".to_vec(),
        ));
        let url = file_url(&path).expect("macOS file URL");
        assert_eq!(url.to_file_path().as_deref(), Some(path.as_path()));
    }

    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut value = Vec::with_capacity(8 + payload.len());
        value.extend_from_slice(&(u32::try_from(8 + payload.len()).unwrap()).to_be_bytes());
        value.extend_from_slice(kind);
        value.extend_from_slice(payload);
        value
    }

    fn movie(sample_count: u32) -> Vec<u8> {
        let mut sample_size = vec![0; 8];
        sample_size.extend_from_slice(&sample_count.to_be_bytes());
        let sample_table = mp4_box(b"stbl", &mp4_box(b"stsz", &sample_size));
        let media = mp4_box(b"mdia", &mp4_box(b"minf", &sample_table));
        mp4_box(b"moov", &mp4_box(b"trak", &media))
    }

    fn fragment(sample_count: u32) -> Vec<u8> {
        let mut run = vec![0; 4];
        run.extend_from_slice(&sample_count.to_be_bytes());
        mp4_box(b"moof", &mp4_box(b"traf", &mp4_box(b"trun", &run)))
    }

    #[test]
    fn numeric_core_media_times_round_trip_through_nanoseconds() {
        // SAFETY: valid CMTime construction.
        let time = unsafe { CMTime::new(3, 2) };
        assert_eq!(time_to_ns(time), Some(1_500_000_000));
        let shifted = shift_time(time, -500_000_000);
        assert_eq!(time_to_ns(shifted), Some(1_000_000_000));
    }

    #[test]
    fn video_samples_without_duration_use_the_configured_frame_interval() {
        assert_eq!(effective_duration_ns(None, 33_333_333), 33_333_333);
        assert_eq!(effective_duration_ns(Some(0), 33_333_333), 33_333_333);
        assert_eq!(
            effective_duration_ns(Some(50_000_000), 33_333_333),
            50_000_000
        );
    }

    #[test]
    fn partial_reasons_accumulate_without_losing_the_original_failure() {
        let mut partial = Some("target disappeared".to_owned());
        append_reason(&mut partial, "finalisation timed out");
        assert_eq!(
            partial.as_deref(),
            Some("target disappeared; finalisation timed out")
        );
    }

    #[test]
    fn mp4_salvage_distinguishes_initialisation_from_playable_media() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("scrozz-mp4-shape-{nonce}.mp4"));
        let mut bytes = mp4_box(b"ftyp", b"isom");
        bytes.extend_from_slice(&movie(0));
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            mp4_salvageability(&path).unwrap(),
            Some(Salvageability::InitialisationOnly)
        );

        bytes.extend_from_slice(&mp4_box(b"mdat", &[1, 2, 3, 4]));
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            mp4_salvageability(&path).unwrap(),
            Some(Salvageability::InitialisationOnly),
            "media bytes without a sample table or fragment are not playable"
        );

        let mut malformed = mp4_box(b"ftyp", b"isom");
        malformed.extend_from_slice(&movie(0));
        malformed.extend_from_slice(&fragment(1));
        malformed.extend_from_slice(&mp4_box(b"mdat", &[1, 2, 3, 4]));
        std::fs::write(&path, malformed).unwrap();

        assert_eq!(
            mp4_salvageability(&path).unwrap(),
            Some(Salvageability::InitialisationOnly),
            "box counts alone cannot promote an undecodable file to playable"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_structurally_plausible_but_undecodable_asset_writer_sidecar() {
        let path = destination(None).unwrap();
        let mut bytes = mp4_box(b"mdat", &[1, 2, 3, 4]);
        bytes.extend_from_slice(&movie(1));
        std::fs::write(&path, bytes).unwrap();

        assert_eq!(
            mp4_salvageability(&path).unwrap(),
            Some(Salvageability::InitialisationOnly)
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cancellation_recovery_survives_destination_removal() {
        let path = destination(None).unwrap();
        std::fs::write(&path, b"retained fragment").unwrap();
        let recovery = preserve_before_cancellation(&path)
            .unwrap()
            .expect("non-empty destination is preserved");

        std::fs::remove_file(&path).unwrap();
        assert_eq!(std::fs::read(&recovery).unwrap(), b"retained fragment");
        std::fs::remove_file(recovery).unwrap();
    }

    #[test]
    fn promotion_never_replaces_a_destination_that_appeared_mid_recording() {
        let destination = destination(None).unwrap();
        let source = destination.with_extension("working");
        std::fs::write(&source, b"new recording").unwrap();
        std::fs::write(&destination, b"existing recording").unwrap();

        let failure = promote_no_replace(&source, &destination).unwrap_err();
        assert!(failure.contains("without replacing"), "{failure}");
        assert_eq!(std::fs::read(&destination).unwrap(), b"existing recording");
        assert_eq!(std::fs::read(&source).unwrap(), b"new recording");

        std::fs::remove_file(source).unwrap();
        std::fs::remove_file(destination).unwrap();
    }

    #[test]
    fn each_writer_owns_an_isolated_private_sidecar_directory() {
        let destination = destination(None).unwrap();
        let first = WorkingOutput::create(&destination).unwrap();
        let second = WorkingOutput::create(&destination).unwrap();
        assert_ne!(first.directory, second.directory);
        assert_eq!(
            std::fs::metadata(&first.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
