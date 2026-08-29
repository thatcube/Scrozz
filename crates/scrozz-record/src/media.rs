//! Native media inspection and bounded-memory sample decoding.
//!
//! This is the user-real boundary for recorded files. Construction requires a
//! native [`Recording`], validates the file on disk, and delegates decoding to
//! the operating system's media stack. Deterministic fixtures have their own
//! model constructors and cannot create a [`NativeMediaSource`].

use std::{path::Path, time::Duration};

use scrozz_core::{Error, Result};
use scrozz_export::RgbaImage;

use crate::{
    Recording,
    edit::{SourceMetadata, TrimRange},
};

const MAX_DECODED_FRAME_BYTES: usize = 256 * 1024 * 1024;
const PROBE_MAX_EDGE: u32 = 64;

#[cfg(target_os = "macos")]
#[path = "media/macos.rs"]
mod platform;
#[cfg(not(target_os = "macos"))]
mod platform {
    use std::{path::Path, time::Duration};

    use scrozz_core::{Error, Result};

    use super::{DecodedMediaSample, SourceInspection};

    pub(super) const BACKEND_NAME: &str = if cfg!(target_os = "windows") {
        "Windows Media Foundation"
    } else if cfg!(target_os = "linux") {
        "linked FFmpeg/VA-API"
    } else {
        "native media framework"
    };
    pub(super) const AVAILABLE: bool = false;
    pub(super) const UNAVAILABLE_REASON: Option<&str> = Some(if cfg!(target_os = "windows") {
        "the Media Foundation source-reader/writer adapter is not included yet"
    } else if cfg!(target_os = "linux") {
        "the linked libav decoder/export adapter is not included yet"
    } else {
        "this target has no native media adapter"
    });

    pub(super) fn inspect(_path: &Path, _file_size_bytes: u64) -> Result<SourceInspection> {
        Err(unavailable())
    }

    pub(super) struct Decoder;

    impl Decoder {
        pub(super) fn open(
            _path: &Path,
            _start: Duration,
            _end: Duration,
            _fps: f64,
            _dimensions: Option<(u32, u32)>,
        ) -> Result<Self> {
            Err(unavailable())
        }

        pub(super) fn next_sample(&mut self) -> Result<Option<DecodedMediaSample>> {
            Err(unavailable())
        }

        pub(super) fn cancel(&mut self) {}
    }

    fn unavailable() -> Error {
        Error::Unsupported {
            what: "native recording inspection and decoding".to_owned(),
            why: if cfg!(target_os = "windows") {
                "the Media Foundation source-reader backend is not included in this build"
                    .to_owned()
            } else if cfg!(target_os = "linux") {
                "the linked libav decoder backend is not included in this build; Scrozz never invokes an external ffmpeg executable".to_owned()
            } else {
                "this target has no native media backend".to_owned()
            },
        }
    }
}

/// Runtime capabilities for the platform-native media pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeMediaCapabilities {
    /// Native framework intended for this target.
    pub backend: &'static str,
    /// Whether real source inspection and decoding are implemented.
    pub source_decode: bool,
    /// Whether real video transcoding is implemented.
    pub video_transcode: bool,
    /// Whether recording-to-GIF decoding/export is implemented.
    pub gif_transcode: bool,
    /// Explicit implementation gap, when unavailable.
    pub unavailable_reason: Option<&'static str>,
}

/// Reports native media availability without attempting to open a file.
#[must_use]
pub const fn native_media_capabilities() -> NativeMediaCapabilities {
    NativeMediaCapabilities {
        backend: platform::BACKEND_NAME,
        source_decode: platform::AVAILABLE,
        video_transcode: platform::AVAILABLE,
        gif_transcode: platform::AVAILABLE,
        unavailable_reason: platform::UNAVAILABLE_REASON,
    }
}

/// Metadata proven by opening the encoded source file.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceInspection {
    /// Video/audio stream summary.
    pub metadata: SourceMetadata,
    /// Duration reported by the media container.
    pub duration: Duration,
    /// Current on-disk size.
    pub file_size_bytes: u64,
    /// Platform decoder that performed the inspection.
    pub backend: String,
}

impl SourceInspection {
    fn validate(&self) -> Result<()> {
        self.metadata.validate()?;
        if self.duration.is_zero() {
            return Err(Error::Codec(
                "native media source has zero playable duration".to_owned(),
            ));
        }
        if self.file_size_bytes == 0 {
            return Err(Error::Codec(
                "native media source contains no encoded bytes".to_owned(),
            ));
        }
        if self.backend.trim().is_empty() {
            return Err(Error::Platform(
                "native media backend did not identify itself".to_owned(),
            ));
        }
        Ok(())
    }
}

/// One decoded, tightly packed straight-alpha RGBA video frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedVideoFrame {
    /// Presentation timestamp on the source timeline.
    pub timestamp: Duration,
    /// Display duration.
    pub duration: Duration,
    /// Owned RGBA pixels.
    pub image: RgbaImage,
}

/// One decoded interleaved Float32 PCM chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedAudioChunk {
    /// Presentation timestamp on the source timeline.
    pub timestamp: Duration,
    /// Chunk duration.
    pub duration: Duration,
    /// PCM sample rate.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u16,
    /// Normalized samples in `-1.0..=1.0`.
    pub samples: Vec<f32>,
}

/// One sample from a source decoder, ordered by presentation timestamp.
#[derive(Debug, Clone, PartialEq)]
pub enum DecodedMediaSample {
    /// Decoded video.
    Video(DecodedVideoFrame),
    /// Decoded audio.
    Audio(DecodedAudioChunk),
}

impl DecodedMediaSample {
    /// Presentation timestamp on the source timeline.
    #[must_use]
    pub const fn timestamp(&self) -> Duration {
        match self {
            Self::Video(frame) => frame.timestamp,
            Self::Audio(chunk) => chunk.timestamp,
        }
    }
}

/// A validated native recording file.
#[derive(Debug, Clone)]
pub struct NativeMediaSource {
    recording: Recording,
    inspection: SourceInspection,
}

impl NativeMediaSource {
    /// Opens and inspects a real, playable recording.
    ///
    /// # Errors
    ///
    /// Returns an error for synthetic/non-playable provenance, a missing or
    /// non-file path, empty bytes, or media rejected by the native decoder.
    pub fn open(recording: Recording) -> Result<Self> {
        recording.require_native()?;
        if !recording.is_playable() {
            return Err(Error::InvalidRequest(
                "recording output is retained but not playable".to_owned(),
            ));
        }
        let file = std::fs::metadata(recording.path()).map_err(|error| {
            Error::Storage(format!(
                "could not inspect recording {}: {error}",
                recording.path().display()
            ))
        })?;
        if !file.is_file() {
            return Err(Error::InvalidRequest(format!(
                "recording source is not a file: {}",
                recording.path().display()
            )));
        }
        if file.len() == 0 {
            return Err(Error::Codec(format!(
                "recording source is empty: {}",
                recording.path().display()
            )));
        }
        let inspection = platform::inspect(recording.path(), file.len())?;
        inspection.validate()?;
        let source = Self {
            recording,
            inspection,
        };
        source.prove_decodable_video()?;
        Ok(source)
    }

    /// Native recording report that authorized this source.
    #[must_use]
    pub const fn recording(&self) -> &Recording {
        &self.recording
    }

    /// Metadata read from the encoded file.
    #[must_use]
    pub const fn inspection(&self) -> &SourceInspection {
        &self.inspection
    }

    /// Stream metadata read from the encoded file.
    #[must_use]
    pub const fn metadata(&self) -> SourceMetadata {
        self.inspection.metadata
    }

    /// Name of the native media backend available on this target.
    #[must_use]
    pub const fn backend_name() -> &'static str {
        platform::BACKEND_NAME
    }

    /// Starts a bounded-memory decoder over one source interval.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid range or native decoder startup failure.
    pub fn decoder(&self, range: TrimRange) -> Result<NativeMediaDecoder> {
        TrimRange::new(range.start, range.end, self.inspection.duration)?;
        Ok(NativeMediaDecoder {
            inner: platform::Decoder::open(
                self.path(),
                range.start,
                range.end,
                self.inspection.metadata.fps,
                None,
            )?,
        })
    }

    pub(crate) fn decoder_with_dimensions(
        &self,
        range: TrimRange,
        dimensions: (u32, u32),
    ) -> Result<NativeMediaDecoder> {
        TrimRange::new(range.start, range.end, self.inspection.duration)?;
        if dimensions.0 == 0 || dimensions.1 == 0 {
            return Err(Error::InvalidRequest(
                "decoded output dimensions must have area".to_owned(),
            ));
        }
        let bytes = usize::try_from(dimensions.0)
            .ok()
            .and_then(|width| {
                usize::try_from(dimensions.1)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| {
                Error::InvalidRequest("decoded video dimensions overflow memory".to_owned())
            })?;
        if dimensions.0 > SourceMetadata::MAX_EDGE
            || dimensions.1 > SourceMetadata::MAX_EDGE
            || bytes > MAX_DECODED_FRAME_BYTES
        {
            return Err(Error::Unsupported {
                what: format!("{}x{} decoded video", dimensions.0, dimensions.1),
                why: format!(
                    "decoded frames are bounded to {MAX_DECODED_FRAME_BYTES} bytes and {} pixels per edge",
                    SourceMetadata::MAX_EDGE
                ),
            });
        }
        Ok(NativeMediaDecoder {
            inner: platform::Decoder::open(
                self.path(),
                range.start,
                range.end,
                self.inspection.metadata.fps,
                Some(dimensions),
            )?,
        })
    }

    /// Encoded file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.recording.path()
    }

    fn prove_decodable_video(&self) -> Result<()> {
        let range = TrimRange::full(self.inspection.duration)?;
        let dimensions = probe_dimensions(self.inspection.metadata);
        let mut decoder = self.decoder_with_dimensions(range, dimensions)?;
        loop {
            match decoder.next_sample()? {
                Some(DecodedMediaSample::Video(frame))
                    if (frame.image.width, frame.image.height) == dimensions
                        && !frame.image.data.is_empty() =>
                {
                    return Ok(());
                }
                Some(_) => {}
                None => {
                    return Err(Error::Codec(format!(
                        "native media source {} has no decodable video frame",
                        self.path().display()
                    )));
                }
            }
        }
    }
}

fn probe_dimensions(metadata: SourceMetadata) -> (u32, u32) {
    let edge = metadata.width.max(metadata.height);
    if edge <= PROBE_MAX_EDGE {
        return (metadata.width, metadata.height);
    }
    let scale = f64::from(PROBE_MAX_EDGE) / f64::from(edge);
    (
        (f64::from(metadata.width) * scale).round().max(1.0) as u32,
        (f64::from(metadata.height) * scale).round().max(1.0) as u32,
    )
}

/// Pull decoder for one validated native source.
pub struct NativeMediaDecoder {
    inner: platform::Decoder,
}

impl NativeMediaDecoder {
    /// Decodes the next video frame or audio chunk in timestamp order.
    ///
    /// # Errors
    ///
    /// Returns a codec/platform error if native decoding fails.
    pub fn next_sample(&mut self) -> Result<Option<DecodedMediaSample>> {
        self.inner.next_sample()
    }

    /// Stops native decoding and releases decoder read-ahead.
    pub fn cancel(&mut self) {
        self.inner.cancel();
    }
}
