//! Fragmented MP4 writing through AVAssetWriter and VideoToolbox.

use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{io::Read, io::Seek};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterStatus, AVFileTypeMPEG4, AVMediaTypeAudio,
    AVMediaTypeVideo,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags};
use objc2_foundation::{NSString, NSURL};
use scrozz_core::{Error, Result};

use super::mix::LiveMixer;
use super::pcm;
use super::plan::RecordingPlan;
use super::settings;
use super::timeline::SampleTimeline;

const FINISH_TIMEOUT: Duration = Duration::from_secs(30);
const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;

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
    audio_mixer: LiveMixer,
    path: PathBuf,
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
}

impl Writer {
    pub(crate) fn new(
        requested_path: Option<&Path>,
        plan: &RecordingPlan,
        fps: u32,
        system_audio: bool,
        microphone: bool,
    ) -> Result<Self> {
        let path = destination(requested_path)?;
        if path.exists() {
            return Err(Error::InvalidRequest(format!(
                "recording destination already exists: {}",
                path.display()
            )));
        }

        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }

        // SAFETY: immutable weak-linked AVFoundation constant.
        let file_type = unsafe { AVFileTypeMPEG4 }.ok_or_else(|| Error::Unsupported {
            what: "MP4 recording".to_owned(),
            why: "AVFoundation did not expose the MPEG-4 container type".to_owned(),
        })?;
        let path_string = NSString::from_str(&path.to_string_lossy());
        let url = NSURL::fileURLWithPath(&path_string);
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

        Ok(Self {
            asset,
            video,
            audio,
            video_timeline: SampleTimeline::default(),
            audio_timeline: SampleTimeline::default(),
            audio_mixer: LiveMixer::new(system_audio, microphone),
            path,
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
        self.video_timeline.pause();
        self.audio_timeline.pause();
        self.paused = true;
        Ok(())
    }

    pub(crate) fn resume(&mut self) {
        self.paused = false;
    }

    pub(crate) fn video_ready(&self) -> bool {
        !self.paused && unsafe { self.video.isReadyForMoreMediaData() }
    }

    pub(crate) fn append_video(&mut self, sample: &CMSampleBuffer) -> Result<()> {
        if self.paused {
            return Ok(());
        }
        // SAFETY: readiness is a thread-safe property read.
        if !unsafe { self.video.isReadyForMoreMediaData() } {
            self.dropped_frames = self.dropped_frames.saturating_add(1);
            return Ok(());
        }
        let (source_pts, source_ns) = sample_time(sample)?;
        self.ensure_session(source_pts, source_ns);
        let (sample, end_ns) = rebase(sample, &mut self.video_timeline, source_ns)?;
        // SAFETY: the input belongs to this active writer and the sample carries
        // a video image buffer from ScreenCaptureKit.
        if !unsafe { self.video.appendSampleBuffer(&sample) } {
            return Err(writer_failure(&self.asset, "encoding a video frame"));
        }
        self.frames = self.frames.saturating_add(1);
        self.media_end_ns = self
            .media_end_ns
            .max(end_ns.saturating_sub(self.session_origin_ns.unwrap_or(end_ns)));
        Ok(())
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
        let (sample, end_ns) = rebase(&sample, &mut self.audio_timeline, source_ns)?;
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

        if !*done {
            append_reason(
                &mut partial,
                "AVAssetWriter did not finish within 30 seconds",
            );
        } else {
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
            return Err(discard_unplayable(
                &self.path,
                Error::Codec("recording ended before any video frame was encoded".to_owned()),
            ));
        }

        if partial.is_some() {
            match contains_playable_mp4_media(&self.path) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(discard_unplayable(
                        &self.path,
                        Error::Storage(format!(
                            "recording failed before a playable MP4 fragment was written: {}",
                            partial.as_deref().unwrap_or("unknown interruption")
                        )),
                    ));
                }
                Err(failure) => {
                    return Err(discard_unplayable(
                        &self.path,
                        Error::Storage(format!(
                            "recording failed and its MP4 could not be validated: {failure}"
                        )),
                    ));
                }
            }
        }

        Ok(WriterSummary {
            path: self.path.clone(),
            frames: self.frames,
            has_audio: self.has_audio,
            duration: Duration::from_nanos(self.media_end_ns.max(0) as u64),
            partial,
        })
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if !self.finished {
            // SAFETY: this writer is being abandoned before any successful
            // finalization; cancelWriting synchronously releases encoder state
            // and removes the unusable output file.
            unsafe {
                self.asset.cancelWriting();
            }
        }
    }
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
) -> Result<(TimedSample<'a>, i64)> {
    let duration_ns = unsafe { time_to_ns(sample.duration()) }.unwrap_or(0).max(0);
    let output_ns = timeline.map(source_ns, duration_ns);
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

fn contains_playable_mp4_media(path: &Path) -> std::io::Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let file_length = file.metadata()?.len();
    let mut offset = 0_u64;
    let mut has_file_type = false;
    let mut has_movie = false;
    let mut has_media = false;

    while offset.saturating_add(8) <= file_length {
        file.seek(std::io::SeekFrom::Start(offset))?;
        let mut header = [0_u8; 8];
        file.read_exact(&mut header)?;
        let short_size = u32::from_be_bytes(header[..4].try_into().expect("four bytes"));
        let kind: [u8; 4] = header[4..].try_into().expect("four bytes");
        let (box_size, header_size) = match short_size {
            0 => (file_length - offset, 8_u64),
            1 => {
                let mut extended = [0_u8; 8];
                file.read_exact(&mut extended)?;
                (u64::from_be_bytes(extended), 16)
            }
            size => (u64::from(size), 8),
        };
        if box_size < header_size || offset.saturating_add(box_size) > file_length {
            break;
        }
        match &kind {
            b"ftyp" => has_file_type = true,
            b"moov" => has_movie = true,
            b"mdat" if box_size > header_size => has_media = true,
            _ => {}
        }
        if has_file_type && has_movie && has_media {
            return Ok(true);
        }
        if box_size == 0 {
            break;
        }
        offset = offset.saturating_add(box_size);
    }
    Ok(false)
}

fn discard_unplayable(path: &Path, failure: Error) -> Error {
    match std::fs::remove_file(path) {
        Ok(()) => failure,
        Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => failure,
        Err(cleanup) => Error::Storage(format!(
            "{failure}; removing unplayable output {} also failed: {cleanup}",
            path.display()
        )),
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
    use super::*;

    #[test]
    fn numeric_core_media_times_round_trip_through_nanoseconds() {
        // SAFETY: valid CMTime construction.
        let time = unsafe { CMTime::new(3, 2) };
        assert_eq!(time_to_ns(time), Some(1_500_000_000));
        let shifted = shift_time(time, -500_000_000);
        assert_eq!(time_to_ns(shifted), Some(1_000_000_000));
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
    fn mp4_salvage_requires_container_metadata_and_complete_media() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("scrozz-mp4-shape-{nonce}.mp4"));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&12_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"isom");
        bytes.extend_from_slice(&8_u32.to_be_bytes());
        bytes.extend_from_slice(b"moov");
        bytes.extend_from_slice(&12_u32.to_be_bytes());
        bytes.extend_from_slice(b"mdat");
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        std::fs::write(&path, bytes).unwrap();

        assert!(contains_playable_mp4_media(&path).unwrap());
        std::fs::remove_file(path).unwrap();
    }
}
