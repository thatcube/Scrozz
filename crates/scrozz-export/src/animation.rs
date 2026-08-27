//! Reusable animation formats and encoders.
//!
//! Recording export and the headless UI harness both need animated GIFs. The
//! codec lives here so neither caller grows a second, subtly different encoder.

use std::time::Duration;

use image::{
    Delay, Frame as ImageFrame, RgbaImage as ImageRgba,
    codecs::gif::{GifEncoder, Repeat},
};
use scrozz_core::{Error, Result};

use crate::RgbaImage;

/// Smallest delay representable by GIF's centisecond clock.
pub const GIF_MIN_FRAME_DELAY: Duration = Duration::from_millis(10);
/// Largest delay representable by GIF's unsigned 16-bit centisecond field.
pub const GIF_MAX_FRAME_DELAY: Duration = Duration::from_millis(655_350);

/// An output animation format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnimationFormat {
    /// Graphics Interchange Format.
    Gif,
}

impl AnimationFormat {
    /// The conventional file extension, without a dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Gif => "gif",
        }
    }

    /// The IANA media type.
    #[must_use]
    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Gif => "image/gif",
        }
    }
}

/// How an animation repeats after its first play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnimationRepeat {
    /// Play once. This is encoded as zero additional repetitions.
    Once,
    /// Repeat forever.
    #[default]
    Infinite,
    /// Repeat this many additional times after the first play.
    Finite(u16),
}

/// One tightly packed RGBA frame and how long it remains visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedRgbaFrame {
    /// Frame pixels.
    pub image: RgbaImage,
    /// Display time for this frame.
    pub delay: Duration,
}

impl TimedRgbaFrame {
    /// Creates a timed frame.
    #[must_use]
    pub const fn new(image: RgbaImage, delay: Duration) -> Self {
        Self { image, delay }
    }
}

/// Deterministic GIF encoder backed by the `image` crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GifAnimationEncoder {
    repeat: AnimationRepeat,
}

impl GifAnimationEncoder {
    /// Creates an infinitely repeating encoder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            repeat: AnimationRepeat::Infinite,
        }
    }

    /// Creates an encoder with explicit repeat behaviour.
    #[must_use]
    pub const fn with_repeat(repeat: AnimationRepeat) -> Self {
        Self { repeat }
    }

    /// The repeat behaviour in force.
    #[must_use]
    pub const fn repeat(self) -> AnimationRepeat {
        self.repeat
    }

    /// Encodes timed RGBA frames as GIF bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for no frames, malformed pixels,
    /// unequal dimensions, dimensions GIF cannot represent, or delays outside
    /// GIF's centisecond range. Codec failures are returned as [`Error::Codec`].
    pub fn encode(&self, frames: &[TimedRgbaFrame]) -> Result<Vec<u8>> {
        let Some(first) = frames.first() else {
            return Err(Error::InvalidRequest(
                "a GIF animation needs at least one frame".to_owned(),
            ));
        };
        validate_image(&first.image, 0)?;
        if first.image.width > u32::from(u16::MAX) || first.image.height > u32::from(u16::MAX) {
            return Err(Error::InvalidRequest(format!(
                "GIF dimensions {}x{} exceed the 65535x65535 format limit",
                first.image.width, first.image.height
            )));
        }

        let mut encoded = Vec::with_capacity(frames.len());
        for (index, frame) in frames.iter().enumerate() {
            validate_image(&frame.image, index)?;
            if (frame.image.width, frame.image.height) != (first.image.width, first.image.height) {
                return Err(Error::InvalidRequest(format!(
                    "GIF frame {index} is {}x{}, but frame 0 is {}x{}",
                    frame.image.width, frame.image.height, first.image.width, first.image.height
                )));
            }
            if frame.delay < GIF_MIN_FRAME_DELAY || frame.delay > GIF_MAX_FRAME_DELAY {
                return Err(Error::InvalidRequest(format!(
                    "GIF frame {index} delay must be between {} ms and {} ms",
                    GIF_MIN_FRAME_DELAY.as_millis(),
                    GIF_MAX_FRAME_DELAY.as_millis()
                )));
            }
            let millis = frame.delay.as_millis() as u32;
            let image = ImageRgba::from_raw(
                frame.image.width,
                frame.image.height,
                frame.image.data.clone(),
            )
            .ok_or_else(|| {
                Error::InvalidRequest(format!("GIF frame {index} has malformed RGBA pixels"))
            })?;
            encoded.push(ImageFrame::from_parts(
                image,
                0,
                0,
                Delay::from_numer_denom_ms(millis, 1),
            ));
        }

        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            let repeat = match self.repeat {
                AnimationRepeat::Once | AnimationRepeat::Finite(0) => None,
                AnimationRepeat::Infinite => Some(Repeat::Infinite),
                AnimationRepeat::Finite(additional) => Some(Repeat::Finite(additional)),
            };
            if let Some(repeat) = repeat {
                encoder.set_repeat(repeat).map_err(|error| {
                    Error::Codec(format!("GIF repeat encoding failed: {error}"))
                })?;
            }
            encoder
                .encode_frames(encoded)
                .map_err(|error| Error::Codec(format!("GIF encoding failed: {error}")))?;
        }
        Ok(bytes)
    }
}

fn validate_image(image: &RgbaImage, index: usize) -> Result<()> {
    if image.width == 0 || image.height == 0 {
        return Err(Error::InvalidRequest(format!(
            "GIF frame {index} is {}x{}; every frame must have area",
            image.width, image.height
        )));
    }
    let expected = (image.width as usize)
        .checked_mul(image.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| {
            Error::InvalidRequest(format!(
                "GIF frame {index} dimensions overflow a pixel buffer"
            ))
        })?;
    if image.data.len() != expected {
        return Err(Error::InvalidRequest(format!(
            "GIF frame {index} has {} RGBA bytes; {}x{} needs {expected}",
            image.data.len(),
            image.width,
            image.height
        )));
    }
    Ok(())
}
