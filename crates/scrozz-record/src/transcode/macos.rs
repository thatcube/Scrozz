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
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSNumber, NSString, NSURL};
use scrozz_core::{Error, Result};

use crate::{
    Quality, Salvageability,
    macos::{
        error,
        mix::{MIX_SAMPLE_RATE, PcmChunk},
        pcm, settings, writer,
    },
    media::{DecodedAudioChunk, DecodedVideoFrame},
};

const FINISH_TIMEOUT: Duration = Duration::from_secs(30);
const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;
const MAX_ENCODER_RATE_HINT: f64 = 60.0;

pub(super) const TRANSCODER_NAME: &str = "macOS AVFoundation + VideoToolbox";

pub(super) struct VideoWriter {
    asset: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    audio: Option<Retained<AVAssetWriterInput>>,
    path: PathBuf,
    dimensions: (u32, u32),
    encoder_rate_hint: u32,
    video_frames: u64,
    last_video_timestamp: Option<Duration>,
    media_end: Duration,
    finished: bool,
    aborted: bool,
}

impl VideoWriter {
    pub(super) fn new(
        path: &Path,
        dimensions: (u32, u32),
        fps: f64,
        quality: Quality,
        audio_channels: u16,
    ) -> Result<Self> {
        validate_video_configuration(dimensions, fps)?;
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
        let output_fps = encoder_rate_hint(fps);
        let bitrate_fps = bitrate_rate(fps);
        let video_settings = settings::transcode_video(
            dimensions.0,
            dimensions.1,
            bitrate_fps,
            output_fps,
            quality,
        )?;
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
        let pixel_attributes = pixel_buffer_attributes(dimensions);
        let adaptor = unsafe {
            AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(
                &video,
                Some(&pixel_attributes),
            )
        };

        let audio = if audio_channels == 0 {
            None
        } else {
            Some(add_audio(&asset, audio_channels)?)
        };
        unsafe {
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
            dimensions,
            encoder_rate_hint: output_fps,
            video_frames: 0,
            last_video_timestamp: None,
            media_end: Duration::ZERO,
            finished: false,
            aborted: false,
        })
    }

    pub(super) fn append_video(
        &mut self,
        frame: &DecodedVideoFrame,
        source_origin: Duration,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        if let Err(error) = wait_until_ready(&self.asset, &self.video, cancelled) {
            return if error.is_cancellation() {
                Err(error)
            } else {
                Err(self.abort_with(error))
            };
        }
        let pixels = match pixel_buffer(&self.adaptor, &frame.image, self.dimensions) {
            Ok(pixels) => pixels,
            Err(error) => return Err(self.abort_with(error)),
        };
        let timestamp = frame.timestamp.saturating_sub(source_origin);
        if let Some(previous) = self.last_video_timestamp {
            if timestamp == previous {
                return Ok(());
            }
            if timestamp < previous {
                return Err(self.abort_with(Error::Codec(format!(
                    "decoded video timestamp moved backwards from {:.6} to {:.6} seconds",
                    previous.as_secs_f64(),
                    timestamp.as_secs_f64()
                ))));
            }
        }
        if !unsafe {
            self.adaptor
                .appendPixelBuffer_withPresentationTime(&pixels, time_from_duration(timestamp))
        } {
            let error = writer_failure(
                &self.asset,
                &format!(
                    "encoding hardware H.264 video frame {} at {:.6} seconds as NV12 {}x{} ({} fps encoder hint)",
                    self.video_frames + 1,
                    timestamp.as_secs_f64(),
                    self.dimensions.0,
                    self.dimensions.1,
                    self.encoder_rate_hint,
                ),
            );
            return Err(self.abort_with(error));
        }
        self.video_frames = self.video_frames.saturating_add(1);
        self.last_video_timestamp = Some(timestamp);
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
        if let Err(error) = wait_until_ready(&self.asset, input, cancelled) {
            return if error.is_cancellation() {
                Err(error)
            } else {
                Err(self.abort_with(error))
            };
        }
        if !unsafe { input.appendSampleBuffer(&sample) } {
            let error = writer_failure(&self.asset, "encoding an audio chunk");
            return Err(self.abort_with(error));
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

    pub(super) const fn aborted(&self) -> bool {
        self.aborted
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

    fn abort_with(&mut self, error: Error) -> Error {
        if !self.finished {
            unsafe {
                self.asset.cancelWriting();
            }
            self.finished = true;
            self.aborted = true;
        }
        error
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

fn validate_video_configuration(dimensions: (u32, u32), fps: f64) -> Result<()> {
    if dimensions.0 == 0 || dimensions.1 == 0 {
        return Err(Error::InvalidRequest(
            "video export dimensions must have area".to_owned(),
        ));
    }

    if !dimensions.0.is_multiple_of(2) || !dimensions.1.is_multiple_of(2) {
        return Err(Error::Unsupported {
            what: format!("{}x{} H.264 video export", dimensions.0, dimensions.1),
            why: "VideoToolbox hardware H.264 requires even pixel dimensions".to_owned(),
        });
    }
    if !fps.is_finite() || fps <= 0.0 || fps > 240.0 {
        return Err(Error::InvalidRequest(format!(
            "video export frame rate {fps} must be finite and in 0..=240"
        )));
    }
    Ok(())
}

fn encoder_rate_hint(fps: f64) -> u32 {
    fps.round().clamp(1.0, MAX_ENCODER_RATE_HINT) as u32
}

fn bitrate_rate(fps: f64) -> u32 {
    fps.round().clamp(1.0, 240.0) as u32
}

fn pixel_buffer_attributes(dimensions: (u32, u32)) -> Retained<settings::SettingsDictionary> {
    let attributes = settings::SettingsDictionary::new();
    let pixel_format =
        NSNumber::numberWithUnsignedInt(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
    let width = NSNumber::numberWithUnsignedInt(dimensions.0);
    let height = NSNumber::numberWithUnsignedInt(dimensions.1);
    let io_surface = settings::SettingsDictionary::new();
    attributes.insert(
        cf_string_as_ns(unsafe { kCVPixelBufferPixelFormatTypeKey }),
        settings::any(&*pixel_format),
    );
    attributes.insert(
        cf_string_as_ns(unsafe { kCVPixelBufferWidthKey }),
        settings::any(&*width),
    );
    attributes.insert(
        cf_string_as_ns(unsafe { kCVPixelBufferHeightKey }),
        settings::any(&*height),
    );
    attributes.insert(
        cf_string_as_ns(unsafe { kCVPixelBufferIOSurfacePropertiesKey }),
        settings::any(&*io_surface),
    );
    attributes
}

fn pixel_buffer(
    adaptor: &AVAssetWriterInputPixelBufferAdaptor,
    image: &scrozz_export::RgbaImage,
    dimensions: (u32, u32),
) -> Result<CFRetained<CVPixelBuffer>> {
    if (image.width, image.height) != dimensions {
        return Err(Error::Codec(format!(
            "decoded video frame is {}x{}, but the encoder is configured for {}x{}",
            image.width, image.height, dimensions.0, dimensions.1
        )));
    }
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

    let pool = unsafe { adaptor.pixelBufferPool() }.ok_or_else(|| {
        Error::Codec(
            "AVAssetWriter did not create its encoder-compatible pixel buffer pool".to_owned(),
        )
    })?;
    let mut raw = std::ptr::null_mut();
    let status =
        unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut raw)) };
    if status != 0 {
        return Err(Error::Codec(format!(
            "allocating a video encoder pixel buffer failed with status {status}"
        )));
    }
    let raw = NonNull::new(raw)
        .ok_or_else(|| Error::Codec("CoreVideo returned no pixel buffer".to_owned()))?;
    let buffer = unsafe { CFRetained::from_raw(raw) };
    if (
        CVPixelBufferGetWidth(&buffer),
        CVPixelBufferGetHeight(&buffer),
        CVPixelBufferGetPixelFormatType(&buffer),
    ) != (
        width,
        height,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    ) || CVPixelBufferGetPlaneCount(&buffer) != 2
    {
        return Err(Error::Codec(
            "AVAssetWriter pixel buffer pool returned an incompatible frame layout".to_owned(),
        ));
    }
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
    let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, 0);
    let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, 1);
    let y_height = CVPixelBufferGetHeightOfPlane(&buffer, 0);
    let uv_height = CVPixelBufferGetHeightOfPlane(&buffer, 1);
    if y_stride < width || uv_stride < width || y_height != height || uv_height != height / 2 {
        return Err(Error::Codec(format!(
            "AVAssetWriter NV12 pool returned invalid planes: Y {y_stride}x{y_height}, UV {uv_stride}x{uv_height}, expected at least {width}x{height} and {width}x{}",
            height / 2
        )));
    }
    let y_base = NonNull::new(CVPixelBufferGetBaseAddressOfPlane(&buffer, 0).cast::<u8>())
        .ok_or_else(|| Error::Codec("video encoder Y plane has no address".to_owned()))?;
    let uv_base = NonNull::new(CVPixelBufferGetBaseAddressOfPlane(&buffer, 1).cast::<u8>())
        .ok_or_else(|| Error::Codec("video encoder UV plane has no address".to_owned()))?;
    // SAFETY: both planes are locked and were validated for these exact spans.
    let y_plane = unsafe {
        std::slice::from_raw_parts_mut(
            y_base.as_ptr(),
            y_stride
                .checked_mul(y_height)
                .ok_or_else(|| Error::Codec("video encoder Y plane overflowed".to_owned()))?,
        )
    };
    // SAFETY: same as the Y plane above.
    let uv_plane = unsafe {
        std::slice::from_raw_parts_mut(
            uv_base.as_ptr(),
            uv_stride
                .checked_mul(uv_height)
                .ok_or_else(|| Error::Codec("video encoder UV plane overflowed".to_owned()))?,
        )
    };
    rgba_to_nv12(
        &image.data,
        width,
        height,
        y_plane,
        y_stride,
        uv_plane,
        uv_stride,
    )?;
    drop(_lock);
    Ok(buffer)
}

fn rgba_to_nv12(
    rgba: &[u8],
    width: usize,
    height: usize,
    y_plane: &mut [u8],
    y_stride: usize,
    uv_plane: &mut [u8],
    uv_stride: usize,
) -> Result<()> {
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| Error::Codec("RGBA-to-NV12 input dimensions overflowed".to_owned()))?;
    if width == 0
        || height == 0
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || rgba.len() != expected
        || y_stride < width
        || uv_stride < width
        || y_plane.len() < y_stride.saturating_mul(height)
        || uv_plane.len() < uv_stride.saturating_mul(height / 2)
    {
        return Err(Error::Codec(
            "RGBA-to-NV12 conversion received an invalid frame layout".to_owned(),
        ));
    }

    for row in 0..height {
        for column in 0..width {
            let [red, green, blue] = opaque_rgb(rgba, width, column, row);
            y_plane[row * y_stride + column] = video_luma(red, green, blue);
        }
    }
    for row in (0..height).step_by(2) {
        for column in (0..width).step_by(2) {
            let mut red = 0_u32;
            let mut green = 0_u32;
            let mut blue = 0_u32;
            for y in row..row + 2 {
                for x in column..column + 2 {
                    let rgb = opaque_rgb(rgba, width, x, y);
                    red += u32::from(rgb[0]);
                    green += u32::from(rgb[1]);
                    blue += u32::from(rgb[2]);
                }
            }
            let red = ((red + 2) / 4) as u8;
            let green = ((green + 2) / 4) as u8;
            let blue = ((blue + 2) / 4) as u8;
            let offset = (row / 2) * uv_stride + column;
            uv_plane[offset] = video_cb(red, green, blue);
            uv_plane[offset + 1] = video_cr(red, green, blue);
        }
    }
    Ok(())
}

fn opaque_rgb(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 3] {
    let offset = (y * width + x) * 4;
    let alpha = u32::from(rgba[offset + 3]);
    [
        ((u32::from(rgba[offset]) * alpha + 127) / 255) as u8,
        ((u32::from(rgba[offset + 1]) * alpha + 127) / 255) as u8,
        ((u32::from(rgba[offset + 2]) * alpha + 127) / 255) as u8,
    ]
}

fn video_luma(red: u8, green: u8, blue: u8) -> u8 {
    let value =
        (47 * i32::from(red) + 157 * i32::from(green) + 16 * i32::from(blue) + 128) / 256 + 16;
    value.clamp(16, 235) as u8
}

fn video_cb(red: u8, green: u8, blue: u8) -> u8 {
    let value = (-26 * i32::from(red) - 87 * i32::from(green) + 113 * i32::from(blue) + 128)
        .div_euclid(256)
        + 128;
    value.clamp(16, 240) as u8
}

fn video_cr(red: u8, green: u8, blue: u8) -> u8 {
    let value = (112 * i32::from(red) - 102 * i32::from(green) - 10 * i32::from(blue) + 128)
        .div_euclid(256)
        + 128;
    value.clamp(16, 240) as u8
}

fn cf_string_as_ns(value: &objc2_core_foundation::CFString) -> &NSString {
    // SAFETY: CFString and NSString are toll-free bridged on Apple platforms.
    unsafe { &*std::ptr::from_ref(value).cast::<NSString>() }
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
        let recoveries = preserve_writer_candidates(path);
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
            return Err(timeout_failure(
                "AVAssetWriter timed out and cancellation did not quiesce within 30 seconds",
                path,
                recoveries,
            ));
        }
        return Err(timeout_failure(
            "AVAssetWriter timed out; export was cancelled",
            path,
            recoveries,
        ));
    }
    if unsafe { asset.status() } != AVAssetWriterStatus::Completed {
        let failure = writer_failure(asset, "finalizing video export");
        return match restore_playable_writer_output(path, &[]) {
            Ok(_) => Err(failure),
            Err(recovery) => Err(Error::Codec(format!(
                "{}; recovering AVAssetWriter sidecars also failed: {}",
                error_detail(&failure),
                error_detail(&recovery)
            ))),
        };
    }
    Ok(())
}

#[derive(Default)]
struct PreservedWriterCandidates {
    paths: Vec<PathBuf>,
    failures: Vec<String>,
}

impl PreservedWriterCandidates {
    fn record(&mut self, candidate: &Path, result: std::io::Result<Option<PathBuf>>) {
        match result {
            Ok(Some(path)) => self.paths.push(path),
            Ok(None) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => self.failures.push(format!(
                "could not preserve {} before cancelling AVAssetWriter: {error}",
                candidate.display()
            )),
        }
    }
}

fn preserve_writer_candidates(path: &Path) -> PreservedWriterCandidates {
    let mut candidates = vec![path.to_owned()];
    let mut preserved = PreservedWriterCandidates::default();
    match writer::sidecar_paths(path) {
        Ok(sidecars) => candidates.extend(sidecars),
        Err(error) => preserved.failures.push(format!(
            "could not enumerate AVAssetWriter sidecars before cancellation: {error}"
        )),
    }
    for candidate in candidates {
        preserved.record(&candidate, writer::preserve_before_cancellation(&candidate));
    }
    preserved
}

fn timeout_failure(message: &str, path: &Path, preserved: PreservedWriterCandidates) -> Error {
    let recovery = match restore_playable_writer_output(path, &preserved.paths) {
        Ok(true) => "playable partial output was retained".to_owned(),
        Ok(false) => "no playable partial output was produced".to_owned(),
        Err(error) => format!("partial-output recovery failed: {}", error_detail(&error)),
    };
    if preserved.failures.is_empty() {
        Error::Codec(format!("{message}; {recovery}"))
    } else {
        Error::Codec(format!(
            "{message}; {recovery}; some candidates could not be preserved: {}",
            preserved.failures.join("; ")
        ))
    }
}

pub(super) fn restore_playable_writer_output(path: &Path, recoveries: &[PathBuf]) -> Result<bool> {
    restore_playable_writer_output_after_scan(path, recoveries, writer::sidecar_paths(path))
}

pub(super) fn restore_playable_writer_output_after_scan(
    path: &Path,
    recoveries: &[PathBuf],
    sidecar_scan: std::io::Result<Vec<PathBuf>>,
) -> Result<bool> {
    let mut alternatives = recoveries.to_vec();
    let enumeration_error = match sidecar_scan {
        Ok(sidecars) => {
            alternatives.extend(sidecars);
            None
        }
        Err(error) => Some(Error::Storage(format!(
            "could not enumerate AVAssetWriter recovery sidecars: {error}"
        ))),
    };
    alternatives.sort();
    alternatives.dedup();
    let Some((candidate, Salvageability::Playable)) =
        writer::best_retained_output(path, &alternatives)
    else {
        return enumeration_error.map_or(Ok(false), Err);
    };
    if candidate != path {
        promote_recovery(&candidate, path)?;
    }
    for other in alternatives {
        if other == path {
            continue;
        }
        if let Err(error) = std::fs::remove_file(&other)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(Error::Storage(format!(
                "could not remove superseded AVAssetWriter recovery {}: {error}",
                other.display()
            )));
        }
    }
    if let Some(error) = enumeration_error {
        tracing::warn!(%error, "restored AVAssetWriter output from preserved candidates despite sidecar scan failure");
    }
    Ok(true)
}

fn promote_recovery(candidate: &Path, path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::Io(error)),
    }
    match std::fs::rename(candidate, path) {
        Ok(()) => {}
        Err(rename_error) => {
            let mut source = std::fs::File::open(candidate)?;
            let mut destination = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|copy_error| {
                    Error::Storage(format!(
                        "could not promote playable AVAssetWriter recovery {} to {} by rename ({rename_error}) or copy ({copy_error})",
                        candidate.display(),
                        path.display()
                    ))
                })?;
            if let Err(copy_error) =
                std::io::copy(&mut source, &mut destination).and_then(|_| destination.sync_all())
            {
                let _ = std::fs::remove_file(path);
                return Err(Error::Storage(format!(
                    "could not copy playable AVAssetWriter recovery {} to {} after rename failed ({rename_error}): {copy_error}",
                    candidate.display(),
                    path.display()
                )));
            }
        }
    }
    let verified = writer::best_retained_output(path, &[])
        .is_some_and(|(_, salvageability)| salvageability == Salvageability::Playable);
    if !verified {
        let rollback = if candidate.exists() {
            std::fs::remove_file(path)
        } else {
            std::fs::rename(path, candidate)
        };
        return Err(Error::Codec(match rollback {
            Ok(()) => {
                "promoted AVAssetWriter recovery failed verification and was restored".to_owned()
            }
            Err(error) => format!(
                "promoted AVAssetWriter recovery failed verification; restoring {} also failed: {error}",
                candidate.display()
            ),
        }));
    }
    if candidate.exists() {
        std::fs::remove_file(candidate).map_err(|error| {
            Error::Storage(format!(
                "promoted AVAssetWriter recovery but could not remove {}: {error}",
                candidate.display()
            ))
        })?;
    }
    Ok(())
}

fn error_detail(error: &Error) -> String {
    match error {
        Error::Codec(message)
        | Error::Storage(message)
        | Error::Platform(message)
        | Error::InvalidRequest(message)
        | Error::TargetGone(message) => message.clone(),
        _ => error.to_string(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_configuration_rejects_odd_or_unbounded_inputs() {
        assert!(validate_video_configuration((96, 64), 30.0).is_ok());
        assert!(matches!(
            validate_video_configuration((95, 64), 30.0),
            Err(Error::Unsupported { .. })
        ));
        assert!(validate_video_configuration((96, 64), 0.0).is_err());
        assert!(validate_video_configuration((96, 64), 241.0).is_err());
    }

    #[test]
    fn encoder_rate_hint_does_not_repeat_the_recording_timebase_ceiling() {
        assert_eq!(encoder_rate_hint(28.74), 29);
        assert_eq!(encoder_rate_hint(120.0), 60);
        assert_eq!(bitrate_rate(120.0), 120);
        assert_eq!(bitrate_rate(240.0), 240);
    }

    #[test]
    fn rgba_to_nv12_uses_video_range_and_respects_stride() {
        let rgba = [
            0, 0, 0, 255, 255, 255, 255, 255, 255, 0, 0, 255, 0, 0, 255, 255,
        ];
        let mut y = [0xAA; 8];
        let mut uv = [0xAA; 4];
        rgba_to_nv12(&rgba, 2, 2, &mut y, 4, &mut uv, 4).unwrap();
        assert_eq!(&y[..2], &[16, 235]);
        assert!(y[4] > 16);
        assert!(y[5] > 16);
        assert_eq!(&y[2..4], &[0xAA, 0xAA]);
        assert_eq!(&uv[2..], &[0xAA, 0xAA]);
        assert!((16..=240).contains(&uv[0]));
        assert!((16..=240).contains(&uv[1]));
    }

    #[test]
    fn rgba_to_nv12_rejects_mismatched_pixel_bytes() {
        let mut y = [0; 4];
        let mut uv = [0; 2];
        assert!(rgba_to_nv12(&[0; 15], 2, 2, &mut y, 2, &mut uv, 2).is_err());
    }

    #[test]
    fn preservation_keeps_successes_when_a_later_candidate_fails() {
        let mut preserved = PreservedWriterCandidates::default();
        preserved.record(
            Path::new("first.mp4"),
            Ok(Some(PathBuf::from("first.recovery"))),
        );
        preserved.record(
            Path::new("gone.mp4"),
            Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        );
        preserved.record(
            Path::new("blocked.mp4"),
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        );
        assert_eq!(preserved.paths, [PathBuf::from("first.recovery")]);
        assert_eq!(preserved.failures.len(), 1);
        assert!(preserved.failures[0].contains("blocked.mp4"));
    }
}
