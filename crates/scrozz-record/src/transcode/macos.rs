//! AVFoundation/VideoToolbox video output for the native transcoder.

use std::{
    path::{Path, PathBuf},
    ptr::NonNull,
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVAssetWriterStatus,
    AVFileTypeMPEG4, AVMediaTypeAudio, AVMediaTypeVideo,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::CMTime;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSString, NSURL};
use scrozz_core::{Error, Result};

use crate::{
    Quality,
    macos::{
        error,
        mix::{MIX_SAMPLE_RATE, PcmChunk},
        pcm, settings,
    },
    media::{DecodedAudioChunk, DecodedVideoFrame},
};

const FINISH_TIMEOUT: Duration = Duration::from_secs(30);
const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;

pub(super) const TRANSCODER_NAME: &str = "macOS AVFoundation + VideoToolbox";

pub(super) struct VideoWriter {
    asset: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    audio: Option<Retained<AVAssetWriterInput>>,
    path: PathBuf,
    video_frames: u64,
    media_end: Duration,
    finished: bool,
}

impl VideoWriter {
    pub(super) fn new(
        path: &Path,
        dimensions: (u32, u32),
        fps: f64,
        quality: Quality,
        audio_channels: u16,
    ) -> Result<Self> {
        if path.exists() {
            return Err(Error::InvalidRequest(format!(
                "transcode destination already exists: {}",
                path.display()
            )));
        }
        let file_type = unsafe { AVFileTypeMPEG4 }.ok_or_else(|| Error::Unsupported {
            what: "MP4 video export".to_owned(),
            why: "AVFoundation did not expose the MPEG-4 container type".to_owned(),
        })?;
        let path_string = NSString::from_str(&path.to_string_lossy());
        let url = NSURL::fileURLWithPath(&path_string);
        let asset = unsafe {
            AVAssetWriter::assetWriterWithURL_fileType_error(&url, file_type).map_err(
                |failure| Error::Storage(error::describe(&failure, "opening video export")),
            )?
        };

        let video_type = unsafe { AVMediaTypeVideo }.ok_or_else(|| Error::Unsupported {
            what: "video export".to_owned(),
            why: "AVFoundation did not expose the video media type".to_owned(),
        })?;
        let output_fps = fps.round().clamp(1.0, f64::from(u32::MAX)) as u32;
        let video_settings =
            settings::transcode_video(dimensions.0, dimensions.1, output_fps, quality)?;
        if !unsafe { asset.canApplyOutputSettings_forMediaType(Some(&video_settings), video_type) }
        {
            return Err(Error::Unsupported {
                what: "hardware H.264 video export".to_owned(),
                why: format!(
                    "VideoToolbox rejected {}x{} at {output_fps} fps",
                    dimensions.0, dimensions.1
                ),
            });
        }
        let video = unsafe {
            AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                video_type,
                Some(&video_settings),
            )
        };
        unsafe {
            video.setExpectsMediaDataInRealTime(false);
            if !asset.canAddInput(&video) {
                return Err(Error::Codec(
                    "AVAssetWriter refused the transcoded video input".to_owned(),
                ));
            }
            asset.addInput(&video);
        }
        let adaptor = unsafe {
            AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(
                &video,
                None,
            )
        };

        let audio = if audio_channels == 0 {
            None
        } else {
            Some(add_audio(&asset, audio_channels)?)
        };
        unsafe {
            asset.setMovieFragmentInterval(CMTime::new(1, 1));
            asset.setShouldOptimizeForNetworkUse(true);
            if !asset.startWriting() {
                return Err(writer_failure(&asset, "starting hardware video export"));
            }
            asset.startSessionAtSourceTime(CMTime::new(0, 1));
        }

        Ok(Self {
            asset,
            video,
            adaptor,
            audio,
            path: path.to_owned(),
            video_frames: 0,
            media_end: Duration::ZERO,
            finished: false,
        })
    }

    pub(super) fn append_video(
        &mut self,
        frame: &DecodedVideoFrame,
        source_origin: Duration,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        wait_until_ready(&self.asset, &self.video, cancelled)?;
        let pixels = pixel_buffer(&frame.image)?;
        let timestamp = frame.timestamp.saturating_sub(source_origin);
        if !unsafe {
            self.adaptor
                .appendPixelBuffer_withPresentationTime(&pixels, time_from_duration(timestamp))
        } {
            return Err(writer_failure(&self.asset, "encoding a video frame"));
        }
        self.video_frames = self.video_frames.saturating_add(1);
        self.media_end = self.media_end.max(timestamp.saturating_add(frame.duration));
        Ok(())
    }

    pub(super) fn append_audio(
        &mut self,
        chunk: &DecodedAudioChunk,
        source_origin: Duration,
        output_channels: u16,
        gain: f32,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        let Some(input) = &self.audio else {
            return Ok(());
        };
        let relative = chunk.timestamp.saturating_sub(source_origin);
        let start_frame = (relative.as_secs_f64() * f64::from(chunk.sample_rate))
            .round()
            .clamp(i64::MIN as f64, i64::MAX as f64) as i64;
        let mut normalized = PcmChunk {
            start_frame,
            sample_rate: chunk.sample_rate,
            channels: chunk.channels,
            samples: chunk.samples.clone(),
        }
        .to_48khz_channels(output_channels);
        if normalized.samples.is_empty() {
            return Ok(());
        }
        for sample in &mut normalized.samples {
            *sample = (*sample * gain).clamp(-1.0, 1.0);
        }
        let duration =
            Duration::try_from_secs_f64(normalized.frames() as f64 / f64::from(MIX_SAMPLE_RATE))
                .map_err(|_| {
                    Error::Codec("transcoded audio duration is not representable".to_owned())
                })?;
        let sample = pcm::encode_normalized(&normalized)?;
        wait_until_ready(&self.asset, input, cancelled)?;
        if !unsafe { input.appendSampleBuffer(&sample) } {
            return Err(writer_failure(&self.asset, "encoding an audio chunk"));
        }
        self.media_end = self.media_end.max(relative.saturating_add(duration));
        Ok(())
    }

    pub(super) const fn video_frames(&self) -> u64 {
        self.video_frames
    }

    pub(super) const fn media_end(&self) -> Duration {
        self.media_end
    }

    pub(super) fn finish(&mut self, requested_end: Duration) -> Result<u64> {
        if self.finished {
            return Err(Error::InvalidRequest(
                "video transcode writer was already finalized".to_owned(),
            ));
        }
        self.finished = true;
        if self.video_frames == 0 {
            unsafe {
                self.asset.cancelWriting();
            }
            return Err(Error::Codec(
                "video export ended before any video frame was encoded".to_owned(),
            ));
        }

        let end = requested_end
            .min(self.media_end)
            .max(Duration::from_nanos(1));
        unsafe {
            self.asset.endSessionAtSourceTime(time_from_duration(end));
            self.video.markAsFinished();
            if let Some(audio) = &self.audio {
                audio.markAsFinished();
            }
        }
        finish_writer(&self.asset, &self.path)?;
        let file = std::fs::File::open(&self.path)?;
        file.sync_all()?;
        let bytes = file.metadata()?.len();
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(bytes)
    }
}

impl Drop for VideoWriter {
    fn drop(&mut self) {
        if !self.finished {
            unsafe {
                self.asset.cancelWriting();
            }
        }
    }
}

fn add_audio(asset: &AVAssetWriter, channels: u16) -> Result<Retained<AVAssetWriterInput>> {
    let audio_type = unsafe { AVMediaTypeAudio }.ok_or_else(|| Error::Unsupported {
        what: "audio export".to_owned(),
        why: "AVFoundation did not expose the audio media type".to_owned(),
    })?;
    let output_settings = settings::audio_for_channels(channels);
    let input = unsafe {
        AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
            audio_type,
            Some(&output_settings),
        )
    };
    unsafe {
        input.setExpectsMediaDataInRealTime(false);
        if !asset.canAddInput(&input) {
            return Err(Error::Codec(
                "AVAssetWriter refused the transcoded audio input".to_owned(),
            ));
        }
        asset.addInput(&input);
    }
    Ok(input)
}

fn pixel_buffer(image: &scrozz_export::RgbaImage) -> Result<CFRetained<CVPixelBuffer>> {
    let width = usize::try_from(image.width)
        .map_err(|_| Error::Codec("video frame width exceeds usize".to_owned()))?;
    let height = usize::try_from(image.height)
        .map_err(|_| Error::Codec("video frame height exceeds usize".to_owned()))?;
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| Error::Codec("video frame row size overflowed".to_owned()))?;
    let expected = row_bytes
        .checked_mul(height)
        .ok_or_else(|| Error::Codec("video frame size overflowed".to_owned()))?;
    if image.data.len() != expected {
        return Err(Error::Codec(format!(
            "video frame has {} RGBA bytes, expected {expected}",
            image.data.len()
        )));
    }

    let mut raw = std::ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            width,
            height,
            kCVPixelFormatType_32BGRA,
            None,
            NonNull::from(&mut raw),
        )
    };
    if status != 0 {
        return Err(Error::Codec(format!(
            "allocating a video encoder pixel buffer failed with status {status}"
        )));
    }
    let raw = NonNull::new(raw)
        .ok_or_else(|| Error::Codec("CoreVideo returned no pixel buffer".to_owned()))?;
    let buffer = unsafe { CFRetained::from_raw(raw) };
    let flags = CVPixelBufferLockFlags::empty();
    let status = unsafe { CVPixelBufferLockBaseAddress(&buffer, flags) };
    if status != 0 {
        return Err(Error::Codec(format!(
            "locking a video encoder pixel buffer failed with status {status}"
        )));
    }
    let _lock = PixelBufferLock {
        buffer: &buffer,
        flags,
    };
    let stride = CVPixelBufferGetBytesPerRow(&buffer);
    if stride < row_bytes {
        return Err(Error::Codec(format!(
            "video encoder pixel stride {stride} is shorter than {row_bytes}"
        )));
    }
    let base = NonNull::new(CVPixelBufferGetBaseAddress(&buffer).cast::<u8>())
        .ok_or_else(|| Error::Codec("video encoder pixel buffer has no address".to_owned()))?;
    for row in 0..height {
        let source = &image.data[row * row_bytes..(row + 1) * row_bytes];
        // SAFETY: the locked CoreVideo buffer exposes `stride * height` writable
        // bytes and this slice covers only the active part of one row.
        let destination =
            unsafe { std::slice::from_raw_parts_mut(base.as_ptr().add(row * stride), row_bytes) };
        for (rgba, bgra) in source
            .as_chunks::<4>()
            .0
            .iter()
            .zip(destination.as_chunks_mut::<4>().0.iter_mut())
        {
            bgra.copy_from_slice(&[rgba[2], rgba[1], rgba[0], rgba[3]]);
        }
    }
    drop(_lock);
    Ok(buffer)
}

fn wait_until_ready(
    asset: &AVAssetWriter,
    input: &AVAssetWriterInput,
    cancelled: &AtomicBool,
) -> Result<()> {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        if unsafe { input.isReadyForMoreMediaData() } {
            return Ok(());
        }
        if unsafe { asset.status() } != AVAssetWriterStatus::Writing {
            return Err(writer_failure(asset, "waiting for video encoder capacity"));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn finish_writer(asset: &AVAssetWriter, path: &Path) -> Result<()> {
    let completed = Arc::new((Mutex::new(false), Condvar::new()));
    let completion = {
        let completed = Arc::clone(&completed);
        RcBlock::new(move || {
            let (done, ready) = &*completed;
            *done.lock().unwrap_or_else(PoisonError::into_inner) = true;
            ready.notify_all();
        })
    };
    unsafe {
        asset.finishWritingWithCompletionHandler(&completion);
    }
    let (done_mutex, ready) = &*completed;
    let (done, _) = ready
        .wait_timeout_while(
            done_mutex.lock().unwrap_or_else(PoisonError::into_inner),
            FINISH_TIMEOUT,
            |done| !*done,
        )
        .unwrap_or_else(PoisonError::into_inner);
    if !*done {
        drop(done);
        unsafe {
            asset.cancelWriting();
        }
        let (done, _) = ready
            .wait_timeout_while(
                done_mutex.lock().unwrap_or_else(PoisonError::into_inner),
                FINISH_TIMEOUT,
                |done| !*done,
            )
            .unwrap_or_else(PoisonError::into_inner);
        if !*done {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(Error::Storage(format!(
                        "AVAssetWriter timed out, cancellation did not quiesce, and its unstable output could not be removed: {error}"
                    )));
                }
            }
            return Err(Error::Codec(
                "AVAssetWriter timed out and cancellation did not quiesce within 30 seconds; unstable output was discarded"
                    .to_owned(),
            ));
        }
        return Err(Error::Codec(
            "AVAssetWriter timed out; export was cancelled and its incomplete output was discarded"
                .to_owned(),
        ));
    }
    if unsafe { asset.status() } != AVAssetWriterStatus::Completed {
        return Err(writer_failure(asset, "finalizing video export"));
    }
    Ok(())
}

fn writer_failure(asset: &AVAssetWriter, action: &str) -> Error {
    let detail = unsafe {
        asset.error().map_or_else(
            || {
                format!(
                    "{action}: AVAssetWriter ended with status {}",
                    asset.status().0
                )
            },
            |failure| error::describe(&failure, action),
        )
    };
    Error::Codec(detail)
}

fn time_from_duration(duration: Duration) -> CMTime {
    let nanos = i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX);
    unsafe { CMTime::new(nanos, NANOSECONDS_PER_SECOND as i32) }
}

struct PixelBufferLock<'a> {
    buffer: &'a CVPixelBuffer,
    flags: CVPixelBufferLockFlags,
}

impl Drop for PixelBufferLock<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = CVPixelBufferUnlockBaseAddress(self.buffer, self.flags);
        }
    }
}
