//! AVFoundation source inspection and sample decoding.

use std::{path::Path, ptr::NonNull, time::Duration};

use objc2::rc::Retained;
use objc2_av_foundation::{
    AVAsset, AVAssetReader, AVAssetReaderStatus, AVAssetReaderTrackOutput,
    AVAssetReaderVideoCompositionOutput, AVAssetTrack, AVMediaTypeAudio, AVMediaTypeVideo,
    AVMutableVideoComposition,
};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMFormatDescription, CMSampleBuffer, CMTime,
    CMTimeFlags, CMTimeRange,
};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSNumber, NSString, NSURL};
use scrozz_core::{Error, Result};
use scrozz_export::RgbaImage;

use crate::macos::{
    error, pcm,
    settings::{SettingsDictionary, any},
};

use super::{
    DecodedAudioChunk, DecodedMediaSample, DecodedVideoFrame, SourceInspection, SourceMetadata,
};

pub(super) const BACKEND_NAME: &str = "macOS AVFoundation";
pub(super) const AVAILABLE: bool = true;
pub(super) const UNAVAILABLE_REASON: Option<&str> = None;
const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;

pub(super) fn inspect(path: &Path, file_size_bytes: u64) -> Result<SourceInspection> {
    let asset = open_asset(path);
    let duration = duration_from_time(unsafe { asset.duration() }).ok_or_else(|| {
        Error::Codec(format!(
            "AVFoundation reported no numeric duration for {}",
            path.display()
        ))
    })?;
    if duration.is_zero() {
        return Err(Error::Codec(format!(
            "AVFoundation found no playable duration in {}",
            path.display()
        )));
    }

    let video_tracks = tracks(&asset, unsafe { AVMediaTypeVideo }, "video")?;
    let video = first_track(&video_tracks, "video")?;
    if !unsafe { video.isDecodable() } {
        return Err(Error::Codec(format!(
            "the video track in {} is not decodable on this Mac",
            path.display()
        )));
    }
    let natural_size = unsafe { video.naturalSize() };
    let transform = unsafe { video.preferredTransform() };
    let width =
        (transform.a.abs() * natural_size.width + transform.c.abs() * natural_size.height).round();
    let height =
        (transform.b.abs() * natural_size.width + transform.d.abs() * natural_size.height).round();
    let width = finite_dimension(width, "width")?;
    let height = finite_dimension(height, "height")?;

    let mut fps = f64::from(unsafe { video.nominalFrameRate() });
    if !fps.is_finite() || fps <= 0.0 {
        let frame_duration = unsafe { video.minFrameDuration() };
        fps = duration_from_time(frame_duration)
            .filter(|duration| !duration.is_zero())
            .map_or(0.0, |duration| 1.0 / duration.as_secs_f64());
    }
    if !fps.is_finite() || fps <= 0.0 {
        return Err(Error::Codec(format!(
            "AVFoundation could not determine the video frame rate for {}",
            path.display()
        )));
    }

    let audio_tracks = tracks(&asset, unsafe { AVMediaTypeAudio }, "audio")?;
    let audio_channels = if audio_tracks.count() == 0 {
        0
    } else {
        let track = first_track(&audio_tracks, "audio")?;
        probe_audio_channels(&asset, track)?.unwrap_or(audio_track_channels(track)?)
    };

    Ok(SourceInspection {
        metadata: SourceMetadata {
            width,
            height,
            fps,
            audio_channels,
        },
        duration,
        file_size_bytes,
        backend: BACKEND_NAME.to_owned(),
    })
}

pub(super) struct Decoder {
    reader: Retained<AVAssetReader>,
    video: Retained<AVAssetReaderVideoCompositionOutput>,
    audio: Option<Retained<AVAssetReaderTrackOutput>>,
    pending_video: Option<Retained<CMSampleBuffer>>,
    pending_audio: Option<Retained<CMSampleBuffer>>,
    video_done: bool,
    audio_done: bool,
    fallback_frame_duration: Duration,
}

impl Decoder {
    pub(super) fn open(
        path: &Path,
        start: Duration,
        end: Duration,
        fps: f64,
        dimensions: Option<(u32, u32)>,
    ) -> Result<Self> {
        let asset = open_asset(path);
        let reader = unsafe { AVAssetReader::assetReaderWithAsset_error(&asset) }
            .map_err(|failure| Error::Codec(error::describe(&failure, "opening media decoder")))?;
        unsafe {
            reader.setTimeRange(CMTimeRange::new(
                time_from_duration(start),
                time_from_duration(end - start),
            ));
        }

        let video_tracks = tracks(&asset, unsafe { AVMediaTypeVideo }, "video")?;
        first_track(&video_tracks, "video")?;
        let video_settings = video_output_settings(dimensions);
        let video = unsafe {
            AVAssetReaderVideoCompositionOutput::assetReaderVideoCompositionOutputWithVideoTracks_videoSettings(
                &video_tracks,
                Some(&video_settings),
            )
        };
        let composition = video_composition(&asset);
        unsafe {
            video.setVideoComposition(Some(&composition));
            video.setAlwaysCopiesSampleData(false);
            if !reader.canAddOutput(&video) {
                return Err(Error::Codec(
                    "AVAssetReader refused its decoded video output".to_owned(),
                ));
            }
            reader.addOutput(&video);
        }

        let audio_tracks = tracks(&asset, unsafe { AVMediaTypeAudio }, "audio")?;
        let audio = if audio_tracks.count() == 0 {
            None
        } else {
            let output = unsafe {
                AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
                    first_track(&audio_tracks, "audio")?,
                    Some(&audio_output_settings()),
                )
            };
            unsafe {
                output.setAlwaysCopiesSampleData(false);
                if !reader.canAddOutput(&output) {
                    return Err(Error::Codec(
                        "AVAssetReader refused its decoded audio output".to_owned(),
                    ));
                }
                reader.addOutput(&output);
            }
            Some(output)
        };

        if !unsafe { reader.startReading() } {
            return Err(reader_failure(&reader, "starting media decode"));
        }
        let fallback_frame_duration = Duration::try_from_secs_f64(1.0 / fps).map_err(|_| {
            Error::Codec(format!(
                "source frame rate {fps} has no representable duration"
            ))
        })?;
        Ok(Self {
            reader,
            video,
            audio,
            pending_video: None,
            pending_audio: None,
            video_done: false,
            audio_done: audio_tracks.count() == 0,
            fallback_frame_duration,
        })
    }

    pub(super) fn next_sample(&mut self) -> Result<Option<DecodedMediaSample>> {
        self.fill_pending()?;
        let take_video = match (&self.pending_video, &self.pending_audio) {
            (Some(video), Some(audio)) => sample_timestamp(video)? <= sample_timestamp(audio)?,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => return self.end_of_stream(),
        };
        if take_video {
            decode_video(
                self.pending_video
                    .take()
                    .expect("take_video requires a pending video sample"),
                self.fallback_frame_duration,
            )
            .map(Some)
        } else {
            decode_audio(
                self.pending_audio
                    .take()
                    .expect("audio selection requires a pending audio sample"),
            )
            .map(Some)
        }
    }

    pub(super) fn cancel(&mut self) {
        unsafe {
            self.reader.cancelReading();
        }
        self.pending_video = None;
        self.pending_audio = None;
        self.video_done = true;
        self.audio_done = true;
    }

    fn fill_pending(&mut self) -> Result<()> {
        if self.pending_video.is_none() && !self.video_done {
            self.pending_video = unsafe { self.video.copyNextSampleBuffer() };
            self.video_done = self.pending_video.is_none();
        }
        if self.pending_audio.is_none()
            && !self.audio_done
            && let Some(audio) = &self.audio
        {
            self.pending_audio = unsafe { audio.copyNextSampleBuffer() };
            self.audio_done = self.pending_audio.is_none();
        }
        if unsafe { self.reader.status() } == AVAssetReaderStatus::Failed {
            return Err(reader_failure(&self.reader, "decoding media"));
        }
        Ok(())
    }

    fn end_of_stream(&self) -> Result<Option<DecodedMediaSample>> {
        match unsafe { self.reader.status() } {
            AVAssetReaderStatus::Failed => Err(reader_failure(&self.reader, "decoding media")),
            AVAssetReaderStatus::Cancelled => Err(Error::Cancelled),
            _ => Ok(None),
        }
    }
}

fn decode_video(
    sample: Retained<CMSampleBuffer>,
    fallback_duration: Duration,
) -> Result<DecodedMediaSample> {
    let timestamp = sample_timestamp(&sample)?;
    let duration = duration_from_time(unsafe { sample.duration() })
        .filter(|duration| !duration.is_zero())
        .unwrap_or(fallback_duration);
    let image = unsafe { sample.image_buffer() }
        .ok_or_else(|| Error::Codec("decoded video sample has no image buffer".to_owned()))?;
    if CVPixelBufferGetPixelFormatType(&image) != kCVPixelFormatType_32BGRA {
        return Err(Error::Codec(format!(
            "AVFoundation returned pixel format 0x{:08x}, expected BGRA",
            CVPixelBufferGetPixelFormatType(&image)
        )));
    }
    let lock_flags = CVPixelBufferLockFlags::ReadOnly;
    let lock_status = unsafe { CVPixelBufferLockBaseAddress(&image, lock_flags) };
    if lock_status != 0 {
        return Err(Error::Codec(format!(
            "locking decoded video pixels failed with status {lock_status}"
        )));
    }
    let _lock = PixelBufferLock {
        buffer: &image,
        flags: lock_flags,
    };
    let width = CVPixelBufferGetWidth(&image);
    let height = CVPixelBufferGetHeight(&image);
    let stride = CVPixelBufferGetBytesPerRow(&image);
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| Error::Codec("decoded video row size overflowed".to_owned()))?;
    if stride < row_bytes {
        return Err(Error::Codec(format!(
            "decoded video stride {stride} is shorter than its {row_bytes}-byte row"
        )));
    }
    let base = NonNull::new(CVPixelBufferGetBaseAddress(&image).cast::<u8>())
        .ok_or_else(|| Error::Codec("decoded video buffer has no base address".to_owned()))?;
    let capacity = row_bytes
        .checked_mul(height)
        .ok_or_else(|| Error::Codec("decoded video size overflowed memory".to_owned()))?;
    let mut rgba = Vec::with_capacity(capacity);
    for row in 0..height {
        // SAFETY: the locked pixel buffer exposes at least `stride * height`
        // bytes, and only the initialized packed row is read.
        let bytes =
            unsafe { std::slice::from_raw_parts(base.as_ptr().add(row * stride), row_bytes) };
        for pixel in bytes.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    Ok(DecodedMediaSample::Video(DecodedVideoFrame {
        timestamp,
        duration,
        image: RgbaImage {
            width: u32::try_from(width)
                .map_err(|_| Error::Codec("decoded video width exceeds u32".to_owned()))?,
            height: u32::try_from(height)
                .map_err(|_| Error::Codec("decoded video height exceeds u32".to_owned()))?,
            data: rgba,
        },
    }))
}

fn decode_audio(sample: Retained<CMSampleBuffer>) -> Result<DecodedMediaSample> {
    let timestamp = sample_timestamp(&sample)?;
    let pcm = pcm::decode(&sample)?;
    let duration = Duration::try_from_secs_f64(pcm.frames() as f64 / f64::from(pcm.sample_rate))
        .map_err(|_| Error::Codec("decoded audio duration is not representable".to_owned()))?;
    Ok(DecodedMediaSample::Audio(DecodedAudioChunk {
        timestamp,
        duration,
        sample_rate: pcm.sample_rate,
        channels: pcm.channels,
        samples: pcm.samples,
    }))
}

fn probe_audio_channels(asset: &AVAsset, track: &AVAssetTrack) -> Result<Option<u16>> {
    let reader = unsafe { AVAssetReader::assetReaderWithAsset_error(asset) }
        .map_err(|failure| Error::Codec(error::describe(&failure, "probing recording audio")))?;
    let output = unsafe {
        AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
            track,
            Some(&audio_output_settings()),
        )
    };
    unsafe {
        if !reader.canAddOutput(&output) {
            return Err(Error::Codec(
                "AVAssetReader refused audio metadata probing".to_owned(),
            ));
        }
        reader.addOutput(&output);
        if !reader.startReading() {
            return Err(reader_failure(&reader, "starting audio metadata probe"));
        }
    }
    let Some(sample) = (unsafe { output.copyNextSampleBuffer() }) else {
        return match unsafe { reader.status() } {
            AVAssetReaderStatus::Failed => Err(reader_failure(
                &reader,
                "reading the first audio sample for channel metadata",
            )),
            _ => Ok(None),
        };
    };
    Ok(Some(pcm::decode(&sample)?.channels))
}

fn audio_track_channels(track: &AVAssetTrack) -> Result<u16> {
    let descriptions = unsafe { track.formatDescriptions() };
    if descriptions.count() == 0 {
        return Err(Error::Codec(
            "audio track has no format description".to_owned(),
        ));
    }
    // SAFETY: AVFoundation documents these entries as retained
    // CMAudioFormatDescription objects.
    let description = unsafe {
        &*std::ptr::from_ref(descriptions.objectAtIndex_unchecked(0)).cast::<CMFormatDescription>()
    };
    let stream = unsafe {
        CMAudioFormatDescriptionGetStreamBasicDescription(description)
            .as_ref()
            .copied()
    }
    .ok_or_else(|| Error::Codec("audio track has no stream description".to_owned()))?;
    u16::try_from(stream.mChannelsPerFrame)
        .map_err(|_| Error::Codec("audio channel count exceeds u16".to_owned()))
}

fn video_output_settings(dimensions: Option<(u32, u32)>) -> Retained<SettingsDictionary> {
    let settings = SettingsDictionary::new();
    let pixel_format = NSNumber::numberWithUnsignedInt(kCVPixelFormatType_32BGRA);
    settings.insert(
        cf_string_as_ns(unsafe { kCVPixelBufferPixelFormatTypeKey }),
        any(&*pixel_format),
    );
    if let Some((width, height)) = dimensions {
        let width = NSNumber::numberWithUnsignedInt(width);
        let height = NSNumber::numberWithUnsignedInt(height);
        settings.insert(
            cf_string_as_ns(unsafe { kCVPixelBufferWidthKey }),
            any(&*width),
        );
        settings.insert(
            cf_string_as_ns(unsafe { kCVPixelBufferHeightKey }),
            any(&*height),
        );
    }
    settings
}

fn audio_output_settings() -> Retained<SettingsDictionary> {
    const LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");

    let settings = SettingsDictionary::new();
    let format = NSNumber::numberWithUnsignedInt(LINEAR_PCM);
    let format_key = NSString::from_str("AVFormatIDKey");
    settings.insert(&*format_key, any(&*format));
    settings
}

fn open_asset(path: &Path) -> Retained<AVAsset> {
    let path = NSString::from_str(&path.to_string_lossy());
    let url = NSURL::fileURLWithPath(&path);
    unsafe { AVAsset::assetWithURL(&url) }
}

#[allow(deprecated)]
fn video_composition(asset: &AVAsset) -> Retained<AVMutableVideoComposition> {
    unsafe { AVMutableVideoComposition::videoCompositionWithPropertiesOfAsset(asset) }
}

#[allow(deprecated)]
fn tracks(
    asset: &AVAsset,
    media_type: Option<&objc2_av_foundation::AVMediaType>,
    kind: &str,
) -> Result<Retained<NSArray<AVAssetTrack>>> {
    let media_type = media_type.ok_or_else(|| Error::Unsupported {
        what: format!("{kind} decoding"),
        why: format!("AVFoundation did not expose the {kind} media type"),
    })?;
    Ok(unsafe { asset.tracksWithMediaType(media_type) })
}

fn first_track<'a>(tracks: &'a NSArray<AVAssetTrack>, kind: &str) -> Result<&'a AVAssetTrack> {
    if tracks.count() == 0 {
        return Err(Error::Codec(format!("recording contains no {kind} track")));
    }
    // SAFETY: count was checked and the immutable array is retained by the
    // caller for the returned borrow.
    Ok(unsafe { tracks.objectAtIndex_unchecked(0) })
}

fn finite_dimension(value: f64, name: &str) -> Result<u32> {
    if !value.is_finite() || value <= 0.0 || value > f64::from(u32::MAX) {
        return Err(Error::Codec(format!(
            "AVFoundation reported invalid video {name} {value}"
        )));
    }
    Ok(value as u32)
}

fn sample_timestamp(sample: &CMSampleBuffer) -> Result<Duration> {
    duration_from_time(unsafe { sample.presentation_time_stamp() })
        .ok_or_else(|| Error::Codec("decoded media sample has no numeric timestamp".to_owned()))
}

fn duration_from_time(time: CMTime) -> Option<Duration> {
    let value = time.value;
    let timescale = time.timescale;
    let flags = time.flags;
    if !flags.contains(CMTimeFlags::Valid)
        || flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
        || timescale <= 0
        || value < 0
    {
        return None;
    }
    let nanos =
        i128::from(value).checked_mul(i128::from(NANOSECONDS_PER_SECOND))? / i128::from(timescale);
    u64::try_from(nanos).ok().map(Duration::from_nanos)
}

fn time_from_duration(duration: Duration) -> CMTime {
    let nanos = i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX);
    unsafe { CMTime::new(nanos, NANOSECONDS_PER_SECOND as i32) }
}

fn reader_failure(reader: &AVAssetReader, action: &str) -> Error {
    let detail = unsafe {
        reader.error().map_or_else(
            || {
                format!(
                    "{action}: AVAssetReader ended with status {}",
                    reader.status().0
                )
            },
            |failure| error::describe(&failure, action),
        )
    };
    Error::Codec(detail)
}

fn cf_string_as_ns(value: &objc2_core_foundation::CFString) -> &NSString {
    // SAFETY: CFString and NSString are toll-free bridged on Apple platforms.
    unsafe { &*std::ptr::from_ref(value).cast::<NSString>() }
}

struct PixelBufferLock<'a> {
    buffer: &'a objc2_core_video::CVPixelBuffer,
    flags: CVPixelBufferLockFlags,
}

impl Drop for PixelBufferLock<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = CVPixelBufferUnlockBaseAddress(self.buffer, self.flags);
        }
    }
}
