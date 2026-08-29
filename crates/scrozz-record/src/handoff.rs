//! Durable finalized-video handoff for the aggregate capture pipeline.
//!
//! This module deliberately contains no card geometry or rendering. Modern
//! capture-stack code consumes this contract and presents video through the same
//! aggregate card component as screenshots.

use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};

use scrozz_core::{ColorSpace, Error, PixelFormat, Result};

use crate::{
    Recording,
    edit::{SourceMetadata, TrimRange, VideoDocument},
    media::{DecodedMediaSample, DecodedVideoFrame, NativeMediaSource},
};

/// Largest poster edge supplied to a capture card.
pub const POSTER_MAX_EDGE: u32 = 512;

/// Durable aggregate media category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizedMediaKind {
    /// A native recorded video.
    Video,
}

/// Who must keep the finalized file alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizedMediaOwnership {
    /// The application retains this durable file until explicit user deletion.
    ApplicationRetained,
}

/// Aggregate actions that are valid for completed video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizedVideoAction {
    /// Open or foreground the recording editor.
    OpenEditor,
    /// Copy the durable media file where the platform supports file clipboard data.
    CopyFile,
    /// Save/export to another destination.
    SaveAs,
    /// Upload only when an uploader is configured.
    UploadWhenConfigured,
    /// Remove the aggregate card without deleting the durable source.
    CloseCard,
}

/// A bounded decoded poster plus explicit colour interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPoster {
    /// Presentation time represented by this poster.
    pub timestamp: Duration,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Packed bytes per row.
    pub stride: usize,
    /// Pixel channel order.
    pub pixel_format: PixelFormat,
    /// Explicit colour metadata; unknown is preserved rather than guessed.
    pub color_space: ColorSpace,
    /// Straight-alpha RGBA8 bytes.
    pub bytes: Vec<u8>,
}

/// Completed recording ready to enter the modern aggregate capture pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedMediaHandoff {
    /// Canonical durable source file.
    pub path: PathBuf,
    /// Ownership that prevents temporary-file cleanup from removing card media.
    pub ownership: FinalizedMediaOwnership,
    /// Always video for this recording handoff.
    pub media_kind: FinalizedMediaKind,
    /// Bounded first playable frame.
    pub poster: VideoPoster,
    /// Native container duration.
    pub duration: Duration,
    /// Encoded source dimensions.
    pub dimensions: (u32, u32),
    /// Current durable file size.
    pub file_size_bytes: u64,
    /// Whether the source contains captured audio.
    pub audio_present: bool,
    /// Aggregate action that opens this media.
    pub open_action: FinalizedVideoAction,
}

impl FinalizedMediaHandoff {
    /// Builds a handoff from complete native media.
    ///
    /// The recording source is only read. Partial output and known internal
    /// transcode staging paths are rejected so aggregate cards never outlive
    /// cleanup-owned bytes.
    ///
    /// # Errors
    ///
    /// Returns an error for synthetic/partial/unplayable media, missing files,
    /// cleanup-owned staging paths, or media without a decodable poster frame.
    pub fn from_completed(recording: &Recording) -> Result<Self> {
        recording.require_native()?;
        if recording.is_partial() {
            return Err(Error::InvalidRequest(
                "only complete recordings enter the aggregate media handoff".to_owned(),
            ));
        }
        let source = NativeMediaSource::open(recording.clone())?;
        let inspection = source.inspection();
        let dimensions = poster_dimensions(inspection.metadata.width, inspection.metadata.height);
        let mut decoder =
            source.decoder_with_dimensions(TrimRange::full(inspection.duration)?, dimensions)?;
        let frame = loop {
            match decoder.next_sample()? {
                Some(DecodedMediaSample::Video(frame)) => break frame,
                Some(DecodedMediaSample::Audio(_)) => {}
                None => {
                    return Err(Error::Codec(
                        "finalized recording contains no decodable poster frame".to_owned(),
                    ));
                }
            }
        };
        Self::build(recording, inspection.metadata, inspection.duration, frame)
    }

    /// Builds the aggregate handoff from the editor's already-decoded preview.
    ///
    /// This is the application path: it performs no media decode on the UI
    /// thread. The playback worker supplies the frame and this method only
    /// validates durable ownership and bounds the poster.
    ///
    /// # Errors
    ///
    /// Returns an error for partial/synthetic media, missing durable bytes, or
    /// malformed preview pixels.
    pub fn from_preview(document: &VideoDocument, frame: &DecodedVideoFrame) -> Result<Self> {
        Self::build(
            document.recording(),
            document.metadata(),
            document.duration(),
            frame.clone(),
        )
    }

    fn build(
        recording: &Recording,
        metadata: SourceMetadata,
        duration: Duration,
        frame: DecodedVideoFrame,
    ) -> Result<Self> {
        recording.require_native()?;
        if recording.is_partial() {
            return Err(Error::InvalidRequest(
                "only complete recordings enter the aggregate media handoff".to_owned(),
            ));
        }
        let path = std::fs::canonicalize(recording.path()).map_err(|error| {
            Error::Storage(format!(
                "could not canonicalize finalized recording {}: {error}",
                recording.path().display()
            ))
        })?;
        if is_internal_staging_path(&path) {
            return Err(Error::InvalidRequest(format!(
                "finalized recording handoff cannot retain cleanup-owned staging path {}",
                path.display()
            )));
        }
        let file_size_bytes = std::fs::metadata(&path)?.len();
        if file_size_bytes == 0 {
            return Err(Error::Codec(
                "finalized recording handoff source is empty".to_owned(),
            ));
        }
        let poster = Self::poster_from_frame(frame)?;
        Ok(Self {
            path,
            ownership: FinalizedMediaOwnership::ApplicationRetained,
            media_kind: FinalizedMediaKind::Video,
            poster,
            duration,
            dimensions: (metadata.width, metadata.height),
            file_size_bytes,
            audio_present: metadata.audio_channels > 0,
            open_action: FinalizedVideoAction::OpenEditor,
        })
    }

    fn poster_from_frame(frame: DecodedVideoFrame) -> Result<VideoPoster> {
        let expected = usize::try_from(frame.image.width)
            .ok()
            .and_then(|width| {
                usize::try_from(frame.image.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| Error::Codec("video poster size overflowed".to_owned()))?;
        if frame.image.data.len() != expected {
            return Err(Error::Codec(format!(
                "video poster has {} bytes, expected {expected}",
                frame.image.data.len()
            )));
        }
        let (width, height) = poster_dimensions(frame.image.width, frame.image.height);
        let bytes = if (width, height) == (frame.image.width, frame.image.height) {
            frame.image.data
        } else {
            Self::downscale_rgba(
                frame.image.width,
                frame.image.height,
                &frame.image.data,
                width,
                height,
            )
        };
        Ok(VideoPoster {
            timestamp: frame.timestamp,
            width,
            height,
            stride: usize::try_from(width)
                .ok()
                .and_then(|width| width.checked_mul(4))
                .ok_or_else(|| Error::Codec("video poster row size overflowed".to_owned()))?,
            pixel_format: PixelFormat::Rgba8,
            color_space: ColorSpace::Unknown,
            bytes,
        })
    }

    fn downscale_rgba(
        source_width: u32,
        source_height: u32,
        source: &[u8],
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        let mut output = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            let source_y = (u64::from(y) * u64::from(source_height) / u64::from(height)) as u32;
            for x in 0..width {
                let source_x = (u64::from(x) * u64::from(source_width) / u64::from(width)) as u32;
                let offset = ((source_y as usize * source_width as usize) + source_x as usize) * 4;
                output.extend_from_slice(&source[offset..offset + 4]);
            }
        }
        output
    }

    /// Actions the modern aggregate card may expose.
    #[must_use]
    pub const fn actions() -> &'static [FinalizedVideoAction] {
        &[
            FinalizedVideoAction::OpenEditor,
            FinalizedVideoAction::CopyFile,
            FinalizedVideoAction::SaveAs,
            FinalizedVideoAction::UploadWhenConfigured,
            FinalizedVideoAction::CloseCard,
        ]
    }

    /// Durable file used for drag-out and file-oriented actions.
    #[must_use]
    pub fn drag_path(&self) -> &Path {
        &self.path
    }
}

fn poster_dimensions(width: u32, height: u32) -> (u32, u32) {
    let edge = width.max(height);
    if edge <= POSTER_MAX_EDGE {
        return (width, height);
    }
    let scale = f64::from(POSTER_MAX_EDGE) / f64::from(edge);
    (
        (f64::from(width) * scale).round().max(1.0) as u32,
        (f64::from(height) * scale).round().max(1.0) as u32,
    )
}

fn is_internal_staging_path(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(component) = component else {
            return false;
        };
        let component = component.to_string_lossy();
        component.starts_with(".scrozz-transcode-")
            || component.starts_with(".scrozz-transcode-preparing-")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poster_dimensions_preserve_aspect_and_bound_memory() {
        assert_eq!(poster_dimensions(1920, 1080), (512, 288));
        assert_eq!(poster_dimensions(320, 180), (320, 180));
    }

    #[test]
    fn video_actions_exclude_screenshot_only_operations() {
        assert_eq!(
            FinalizedMediaHandoff::actions(),
            &[
                FinalizedVideoAction::OpenEditor,
                FinalizedVideoAction::CopyFile,
                FinalizedVideoAction::SaveAs,
                FinalizedVideoAction::UploadWhenConfigured,
                FinalizedVideoAction::CloseCard,
            ]
        );
    }

    #[test]
    fn cleanup_owned_paths_are_never_handed_to_cards() {
        assert!(is_internal_staging_path(Path::new(
            "/tmp/.scrozz-transcode-7/output.mp4"
        )));
        assert!(!is_internal_staging_path(Path::new(
            "/Users/example/Movies/Scrozz/recording.mp4"
        )));
    }
}
