//! Reusable animation formats and encoders.
//!
//! Recording export and the headless UI harness both need animated GIFs. The
//! codec lives here so neither caller grows a second, subtly different encoder.

use std::{
    borrow::Cow,
    collections::BTreeSet,
    io::{BufReader, Write},
    path::Path,
    time::Duration,
};

use color_quant::NeuQuant;
use gif::{DecodeOptions, Encoder as GifEncoder, Frame as GifFrame, Repeat};
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

/// Palette error diffusion applied before GIF indexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GifDither {
    /// Map each pixel directly to the nearest generated palette entry.
    None,
    /// Diffuse quantization error across neighboring pixels.
    #[default]
    FloydSteinberg,
}

/// Stream-derived properties of a GIF file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GifInspection {
    /// Logical screen width.
    pub width: u16,
    /// Logical screen height.
    pub height: u16,
    /// Number of fully decoded frames.
    pub frames: u64,
    /// Sum of encoded centisecond delays.
    pub duration: Duration,
    /// Loop behavior encoded in the application extension.
    pub repeat: AnimationRepeat,
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

/// Deterministic GIF encoder backed by the streaming `gif` crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GifAnimationEncoder {
    repeat: AnimationRepeat,
    speed: i32,
    dither: GifDither,
}

impl GifAnimationEncoder {
    /// Balanced NeuQuant speed used by the default encoder.
    pub const DEFAULT_SPEED: i32 = 10;

    /// Creates an infinitely repeating encoder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            repeat: AnimationRepeat::Infinite,
            speed: Self::DEFAULT_SPEED,
            dither: GifDither::FloydSteinberg,
        }
    }

    /// Creates an encoder with explicit repeat behaviour.
    #[must_use]
    pub const fn with_repeat(repeat: AnimationRepeat) -> Self {
        Self {
            repeat,
            speed: Self::DEFAULT_SPEED,
            dither: GifDither::FloydSteinberg,
        }
    }

    /// Creates an encoder with explicit repeat and NeuQuant speed.
    ///
    /// `1` spends the most CPU for the best palette, while `30` is fastest.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] unless `speed` is in `1..=30`.
    pub fn with_speed(repeat: AnimationRepeat, speed: i32) -> Result<Self> {
        if !(1..=30).contains(&speed) {
            return Err(Error::InvalidRequest(format!(
                "GIF palette speed {speed} is outside 1..=30"
            )));
        }
        Ok(Self {
            repeat,
            speed,
            dither: GifDither::FloydSteinberg,
        })
    }

    /// Creates an encoder with explicit repeat, palette effort, and dithering.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] unless `speed` is in `1..=30`.
    pub fn with_options(repeat: AnimationRepeat, speed: i32, dither: GifDither) -> Result<Self> {
        let mut encoder = Self::with_speed(repeat, speed)?;
        encoder.dither = dither;
        Ok(encoder)
    }

    /// The repeat behaviour in force.
    #[must_use]
    pub const fn repeat(self) -> AnimationRepeat {
        self.repeat
    }

    /// Palette error diffusion in force.
    #[must_use]
    pub const fn dither(self) -> GifDither {
        self.dither
    }

    /// Starts a bounded-memory stream that writes each frame immediately.
    pub fn stream<W: Write>(&self, writer: W) -> GifAnimationStream<W> {
        GifAnimationStream {
            writer: Some(writer),
            encoder: None,
            repeat: self.repeat,
            speed: self.speed,
            dither: self.dither,
            dimensions: None,
            elapsed_nanos: 0,
            emitted_centiseconds: 0,
            frame_count: 0,
        }
    }

    /// Encodes timed RGBA frames as GIF bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for no frames, malformed pixels,
    /// unequal dimensions, dimensions GIF cannot represent, or delays outside
    /// GIF's centisecond range. Codec failures are returned as [`Error::Codec`].
    pub fn encode(&self, frames: &[TimedRgbaFrame]) -> Result<Vec<u8>> {
        let mut stream = self.stream(Vec::new());
        for frame in frames {
            // The compatibility API borrows a slice, so one frame is cloned at a
            // time. The stream never retains prior RGBA buffers.
            stream.write_frame(frame.clone())?;
        }
        stream.finish()
    }
}

impl Default for GifAnimationEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Validates a GIF from a buffered file stream without loading the artifact.
///
/// Every frame is LZW-decoded so this proves more than a header sniff, while
/// memory remains bounded by the decoder's current-frame buffer.
///
/// # Errors
///
/// Returns [`Error::Io`] if the file cannot be read or [`Error::Codec`] for a
/// malformed, truncated, empty, or inconsistent GIF.
pub fn inspect_gif_file(path: &Path) -> Result<GifInspection> {
    let file = std::fs::File::open(path)?;
    let mut options = DecodeOptions::new();
    options.check_frame_consistency(true);
    let mut decoder = options
        .read_info(BufReader::new(file))
        .map_err(|error| Error::Codec(format!("could not read GIF header: {error}")))?;
    let width = decoder.width();
    let height = decoder.height();
    let repeat = match decoder.repeat() {
        Repeat::Infinite => AnimationRepeat::Infinite,
        Repeat::Finite(0) => AnimationRepeat::Once,
        Repeat::Finite(additional) => AnimationRepeat::Finite(additional),
    };
    let mut frames = 0_u64;
    let mut centiseconds = 0_u64;
    while let Some(frame) = decoder
        .read_next_frame()
        .map_err(|error| Error::Codec(format!("could not decode GIF frame: {error}")))?
    {
        frames = frames.saturating_add(1);
        centiseconds = centiseconds
            .checked_add(u64::from(frame.delay))
            .ok_or_else(|| Error::Codec("GIF duration overflowed u64".to_owned()))?;
    }
    if frames == 0 {
        return Err(Error::Codec(
            "GIF contains no decodable animation frame".to_owned(),
        ));
    }
    Ok(GifInspection {
        width,
        height,
        frames,
        duration: Duration::from_millis(centiseconds.saturating_mul(10)),
        repeat,
    })
}

/// A GIF writer that owns at most the current RGBA frame.
pub struct GifAnimationStream<W: Write> {
    writer: Option<W>,
    encoder: Option<GifEncoder<W>>,
    repeat: AnimationRepeat,
    speed: i32,
    dither: GifDither,
    dimensions: Option<(u32, u32)>,
    elapsed_nanos: u128,
    emitted_centiseconds: u128,
    frame_count: usize,
}

impl<W: Write> GifAnimationStream<W> {
    /// Quantizes and writes one owned frame immediately.
    ///
    /// Delay quantization is cumulative rather than frame-local. For example,
    /// two 15 ms frames become 20 ms + 10 ms, preserving their 30 ms total
    /// instead of independently truncating both to 10 ms.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for malformed frames or a delay that
    /// cannot be represented, and [`Error::Codec`] for encoder/write failures.
    pub fn write_frame(&mut self, frame: TimedRgbaFrame) -> Result<()> {
        let index = self.frame_count;
        validate_image(&frame.image, index)?;
        validate_delay(frame.delay, index)?;
        let dimensions = (frame.image.width, frame.image.height);
        if let Some(expected) = self.dimensions {
            if dimensions != expected {
                return Err(Error::InvalidRequest(format!(
                    "GIF frame {index} is {}x{}, but frame 0 is {}x{}",
                    dimensions.0, dimensions.1, expected.0, expected.1
                )));
            }
        } else {
            self.start(dimensions)?;
        }

        let delay = self.quantized_delay(frame.delay, index)?;
        let width = u16::try_from(frame.image.width).map_err(|_| {
            Error::InvalidRequest(format!(
                "GIF frame {index} width {} exceeds 65535",
                frame.image.width
            ))
        })?;
        let height = u16::try_from(frame.image.height).map_err(|_| {
            Error::InvalidRequest(format!(
                "GIF frame {index} height {} exceeds 65535",
                frame.image.height
            ))
        })?;
        let mut encoded =
            Self::quantize_frame(width, height, frame.image.data, self.speed, self.dither);
        encoded.delay = delay;
        self.encoder
            .as_mut()
            .expect("the first validated frame starts the encoder")
            .write_frame(&encoded)
            .map_err(|error| Error::Codec(format!("GIF frame {index} encoding failed: {error}")))?;
        self.frame_count += 1;
        Ok(())
    }

    fn quantize_frame(
        width: u16,
        height: u16,
        mut rgba: Vec<u8>,
        speed: i32,
        dither: GifDither,
    ) -> GifFrame<'static> {
        if dither == GifDither::None {
            return GifFrame::from_rgba_speed(width, height, &mut rgba, speed);
        }

        if rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 255) {
            return GifFrame::from_rgba_speed(width, height, &mut rgba, speed);
        }
        let mut exact_colors = BTreeSet::new();
        for pixel in rgba.as_chunks::<4>().0 {
            exact_colors.insert([pixel[0], pixel[1], pixel[2], pixel[3]]);
            if exact_colors.len() > 256 {
                break;
            }
        }
        if exact_colors.len() <= 256 {
            return GifFrame::from_rgba_speed(width, height, &mut rgba, speed);
        }

        let palette = NeuQuant::new(speed, 256, &rgba);
        let width_usize = usize::from(width);
        let row_channels = (width_usize + 2) * 3;
        let mut current_error = vec![0_i32; row_channels];
        let mut next_error = vec![0_i32; row_channels];
        let mut indices = Vec::with_capacity(width_usize * usize::from(height));

        for row in rgba.chunks_exact(width_usize * 4) {
            for (x, pixel) in row.as_chunks::<4>().0.iter().enumerate() {
                let error_offset = (x + 1) * 3;
                let adjusted = [
                    (i32::from(pixel[0]) + current_error[error_offset] / 16).clamp(0, 255) as u8,
                    (i32::from(pixel[1]) + current_error[error_offset + 1] / 16).clamp(0, 255)
                        as u8,
                    (i32::from(pixel[2]) + current_error[error_offset + 2] / 16).clamp(0, 255)
                        as u8,
                    255,
                ];
                let index = palette.index_of(&adjusted);
                indices.push(index as u8);
                let mapped = palette.lookup(index).unwrap_or([0, 0, 0, 255]);
                for channel in 0..3 {
                    let error = i32::from(adjusted[channel]) - i32::from(mapped[channel]);
                    current_error[error_offset + 3 + channel] += error * 7;
                    next_error[error_offset - 3 + channel] += error * 3;
                    next_error[error_offset + channel] += error * 5;
                    next_error[error_offset + 3 + channel] += error;
                }
            }
            std::mem::swap(&mut current_error, &mut next_error);
            next_error.fill(0);
        }

        GifFrame {
            width,
            height,
            palette: Some(palette.color_map_rgb()),
            buffer: Cow::Owned(indices),
            ..GifFrame::default()
        }
    }

    /// Centiseconds the cumulative GIF clock would allocate to `additional`.
    ///
    /// This does not mutate the stream. Callers that buffer a final short frame
    /// use it to decide whether the cumulative clock can represent two distinct
    /// images rather than folding the tail away.
    pub fn projected_centiseconds(&self, additional: Duration) -> Result<u128> {
        const CENTISECOND_NANOS: u128 = 10_000_000;
        const HALF_CENTISECOND_NANOS: u128 = CENTISECOND_NANOS / 2;
        let elapsed = self
            .elapsed_nanos
            .checked_add(additional.as_nanos())
            .ok_or_else(|| Error::InvalidRequest("GIF timeline duration overflowed".to_owned()))?;
        let target = elapsed
            .checked_add(HALF_CENTISECOND_NANOS)
            .ok_or_else(|| Error::InvalidRequest("GIF timeline duration overflowed".to_owned()))?
            / CENTISECOND_NANOS;
        target
            .checked_sub(self.emitted_centiseconds)
            .ok_or_else(|| Error::InvalidRequest("GIF timeline regressed".to_owned()))
    }

    /// Writes the GIF trailer and returns the underlying writer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if no frame was written, or
    /// [`Error::Codec`] if finalization failed.
    pub fn finish(mut self) -> Result<W> {
        let encoder = self.encoder.take().ok_or_else(|| {
            Error::InvalidRequest("a GIF animation needs at least one frame".to_owned())
        })?;
        encoder
            .into_inner()
            .map_err(|error| Error::Codec(format!("GIF finalization failed: {error}")))
    }

    fn start(&mut self, dimensions: (u32, u32)) -> Result<()> {
        let width = u16::try_from(dimensions.0).map_err(|_| {
            Error::InvalidRequest(format!(
                "GIF dimensions {}x{} exceed the 65535x65535 format limit",
                dimensions.0, dimensions.1
            ))
        })?;
        let height = u16::try_from(dimensions.1).map_err(|_| {
            Error::InvalidRequest(format!(
                "GIF dimensions {}x{} exceed the 65535x65535 format limit",
                dimensions.0, dimensions.1
            ))
        })?;
        let writer = self.writer.take().expect("the encoder can only start once");
        let mut encoder = GifEncoder::new(writer, width, height, &[])
            .map_err(|error| Error::Codec(format!("GIF header encoding failed: {error}")))?;
        let repeat = match self.repeat {
            AnimationRepeat::Once | AnimationRepeat::Finite(0) => None,
            AnimationRepeat::Infinite => Some(Repeat::Infinite),
            AnimationRepeat::Finite(additional) => Some(Repeat::Finite(additional)),
        };
        if let Some(repeat) = repeat {
            encoder
                .set_repeat(repeat)
                .map_err(|error| Error::Codec(format!("GIF repeat encoding failed: {error}")))?;
        }
        self.dimensions = Some(dimensions);
        self.encoder = Some(encoder);
        Ok(())
    }

    fn quantized_delay(&mut self, delay: Duration, index: usize) -> Result<u16> {
        const CENTISECOND_NANOS: u128 = 10_000_000;
        const HALF_CENTISECOND_NANOS: u128 = CENTISECOND_NANOS / 2;

        self.elapsed_nanos = self
            .elapsed_nanos
            .checked_add(delay.as_nanos())
            .ok_or_else(|| Error::InvalidRequest("GIF timeline duration overflowed".to_owned()))?;
        let target_centiseconds = self
            .elapsed_nanos
            .checked_add(HALF_CENTISECOND_NANOS)
            .ok_or_else(|| Error::InvalidRequest("GIF timeline duration overflowed".to_owned()))?
            / CENTISECOND_NANOS;
        let frame_centiseconds = target_centiseconds
            .checked_sub(self.emitted_centiseconds)
            .ok_or_else(|| Error::InvalidRequest("GIF timeline regressed".to_owned()))?;
        let frame_centiseconds = u16::try_from(frame_centiseconds).map_err(|_| {
            Error::InvalidRequest(format!(
                "GIF frame {index} delay exceeds the 65535-centisecond format limit after cumulative quantization"
            ))
        })?;
        if frame_centiseconds == 0 {
            return Err(Error::InvalidRequest(format!(
                "GIF frame {index} delay quantized below one centisecond"
            )));
        }
        self.emitted_centiseconds = target_centiseconds;
        Ok(frame_centiseconds)
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

fn validate_delay(delay: Duration, index: usize) -> Result<()> {
    if delay.is_zero() || delay > GIF_MAX_FRAME_DELAY {
        return Err(Error::InvalidRequest(format!(
            "GIF frame {index} delay must be positive and no more than {} ms",
            GIF_MAX_FRAME_DELAY.as_millis()
        )));
    }
    Ok(())
}
