//! Video format negotiation, and the honest mapping to Scrozz's pixel types.
//!
//! Everything here is arithmetic and byte layout. None of it needs PipeWire to
//! be installed, none of it needs a compositor, and all of it is where the bugs
//! in a capture backend actually live — so it is a separate, pure module that
//! the cross-platform test binary compiles and runs on macOS and Windows too.
//!
//! # What is offered, and what is deliberately not
//!
//! Scrozz offers `BGRx` and `RGBx`. They map to a [`PixelFormat`] **without
//! reordering bytes**, and their unused fourth byte can be made deterministically
//! opaque. SPA does not describe whether a compositor's RGBA/BGRA pixels are
//! straight or premultiplied, so advertising those formats would force Scrozz to
//! lie about alpha association.
//!
//! `xRGB`, `xBGR`, `ARGB` and `ABGR` are not offered. Scrozz has no pixel format
//! with the alpha byte first, so accepting one would mean either a per-pixel
//! rotate or — far worse — declaring it `Bgra8` and shipping an image whose blue
//! and alpha channels are swapped. A compositor that can only produce those is
//! better met with a negotiation failure it can report than with wrong colour.
//!
//! `SPA_FORMAT_VIDEO_modifier` is also deliberately absent. After PipeWire
//! fixates that modifier-less offer, [`shared_memory_buffer_param`] explicitly
//! limits buffer allocation to `MemFd` and `MemPtr`. Both steps are required by
//! PipeWire's DMA-BUF negotiation contract; merely omitting the modifier leaves
//! the memory type underspecified. Shared memory costs a GPU read-back, which for
//! one still frame is unmeasurable, and it buys freedom from EGL, GBM, `libdrm`
//! and per-driver import quirks. A recorder would want the opposite trade; a
//! screenshot tool does not.
//!
//! # Alpha, told truthfully
//!
//! SPA has no alpha-association metadata, and real compositors disagree about
//! what `BGRA` means. Those formats therefore are not offered. The `x` variants
//! carry an *undefined* fourth byte — not zero, not 255, whatever was in the
//! compositor's scratch buffer — so [`pack_rows`] overwrites it with `0xFF`.

use scrozz_core::{ColorSpace, PixelFormat};

use std::borrow::Cow;

use super::pod::{Choice, Object, Property, Scalar};

/// `SPA_TYPE_OBJECT_Format`.
pub const OBJECT_FORMAT: u32 = 0x0004_0003;
/// `SPA_TYPE_OBJECT_ParamBuffers`.
pub const OBJECT_PARAM_BUFFERS: u32 = 0x0004_0004;

/// Largest dimension offered during negotiation.
///
/// A server may only fixate inside the offered range. Checking the answer keeps
/// all later stride and allocation arithmetic bounded even if the peer is
/// buggy.
pub const MAX_DIMENSION: u32 = 16_384;

/// Parameter ids from `enum spa_param_type`.
pub mod param {
    /// `SPA_PARAM_EnumFormat` — the formats a client can accept.
    pub const ENUM_FORMAT: u32 = 3;
    /// `SPA_PARAM_Format` — the format that was agreed.
    pub const FORMAT: u32 = 4;
    /// `SPA_PARAM_Buffers`.
    pub const BUFFERS: u32 = 5;
    /// `SPA_PARAM_Meta`.
    pub const META: u32 = 6;
}

/// Property keys inside a `Format` object, from `enum spa_format`.
pub mod key {
    /// `SPA_FORMAT_mediaType`.
    pub const MEDIA_TYPE: u32 = 1;
    /// `SPA_FORMAT_mediaSubtype`.
    pub const MEDIA_SUBTYPE: u32 = 2;
    /// `SPA_FORMAT_VIDEO_format`.
    pub const VIDEO_FORMAT: u32 = 0x0002_0001;
    /// `SPA_FORMAT_VIDEO_modifier` — never sent; see the module documentation.
    pub const VIDEO_MODIFIER: u32 = 0x0002_0002;
    /// `SPA_FORMAT_VIDEO_size`.
    pub const VIDEO_SIZE: u32 = 0x0002_0003;
    /// `SPA_FORMAT_VIDEO_framerate`.
    pub const VIDEO_FRAMERATE: u32 = 0x0002_0004;
    /// `SPA_FORMAT_VIDEO_maxFramerate`.
    pub const VIDEO_MAX_FRAMERATE: u32 = 0x0002_0005;
    /// `SPA_FORMAT_VIDEO_transferFunction`.
    pub const VIDEO_TRANSFER: u32 = 0x0002_000E;
    /// `SPA_FORMAT_VIDEO_colorPrimaries`.
    pub const VIDEO_PRIMARIES: u32 = 0x0002_000F;
}

/// Property keys inside a `ParamBuffers` object.
pub mod buffer_key {
    /// `SPA_PARAM_BUFFERS_dataType`.
    pub const DATA_TYPE: u32 = 6;
}

/// `enum spa_data_type` values used by the negotiated buffer policy.
pub mod data_type {
    /// `SPA_DATA_MemPtr`.
    pub const MEM_PTR: u32 = 1;
    /// `SPA_DATA_MemFd`.
    pub const MEM_FD: u32 = 2;
    /// `SPA_DATA_DmaBuf`.
    pub const DMA_BUF: u32 = 3;

    /// Flags-choice mask limiting allocation to shared-memory types.
    pub const SHARED_MEMORY_MASK: i32 = (1_i32 << MEM_PTR) | (1_i32 << MEM_FD);
}

/// `SPA_MEDIA_TYPE_video`.
pub const MEDIA_TYPE_VIDEO: u32 = 2;
/// `SPA_MEDIA_SUBTYPE_raw`.
pub const MEDIA_SUBTYPE_RAW: u32 = 1;

/// `SPA_CHUNK_FLAG_CORRUPTED`.
pub const CHUNK_FLAG_CORRUPTED: u32 = 1 << 0;
/// `SPA_CHUNK_FLAG_EMPTY`.
pub const CHUNK_FLAG_EMPTY: u32 = 1 << 1;

/// How a PipeWire chunk should be treated before its memory is inspected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkDisposition {
    /// No pixels are present; keep waiting even if the compositor also marked
    /// this priming buffer corrupted.
    Empty,
    /// Bytes are present but the compositor says they are invalid.
    Corrupted,
    /// The chunk may contain a frame.
    Data,
}

/// Classifies empty priming chunks before corruption.
#[must_use]
pub const fn chunk_disposition(size: u32, flags: u32) -> ChunkDisposition {
    if size == 0 || flags & CHUNK_FLAG_EMPTY != 0 {
        ChunkDisposition::Empty
    } else if flags & CHUNK_FLAG_CORRUPTED != 0 {
        ChunkDisposition::Corrupted
    } else {
        ChunkDisposition::Data
    }
}

/// The `spa_video_format` values Scrozz accepts, in preference order.
pub mod video_format {
    /// `SPA_VIDEO_FORMAT_RGBx` — bytes R, G, B, undefined.
    pub const RGBX: u32 = 7;
    /// `SPA_VIDEO_FORMAT_BGRx` — bytes B, G, R, undefined.
    pub const BGRX: u32 = 8;
    /// `SPA_VIDEO_FORMAT_RGBA` — bytes R, G, B, A; association is unspecified.
    pub const RGBA: u32 = 11;
    /// `SPA_VIDEO_FORMAT_BGRA` — bytes B, G, R, A; association is unspecified.
    pub const BGRA: u32 = 12;
}

/// `enum spa_video_color_primaries`, for the members Scrozz can name.
pub mod primaries {
    /// `SPA_VIDEO_COLOR_PRIMARIES_UNKNOWN`.
    pub const UNKNOWN: u32 = 0;
    /// `SPA_VIDEO_COLOR_PRIMARIES_BT709` — the sRGB primaries.
    pub const BT709: u32 = 1;
    /// `SPA_VIDEO_COLOR_PRIMARIES_BT2020`.
    pub const BT2020: u32 = 7;
    /// `SPA_VIDEO_COLOR_PRIMARIES_SMPTERP431` — DCI-P3.
    pub const SMPTE_RP431: u32 = 10;
    /// `SPA_VIDEO_COLOR_PRIMARIES_SMPTEEG432` — Display P3.
    pub const SMPTE_EG432: u32 = 11;
}

/// `enum spa_video_transfer_function` values Scrozz can interpret exactly.
pub mod transfer {
    /// `SPA_VIDEO_TRANSFER_UNKNOWN`.
    pub const UNKNOWN: u32 = 0;
    /// `SPA_VIDEO_TRANSFER_GAMMA22`.
    pub const GAMMA22: u32 = 4;
    /// `SPA_VIDEO_TRANSFER_BT709`.
    pub const BT709: u32 = 5;
    /// `SPA_VIDEO_TRANSFER_SRGB`.
    pub const SRGB: u32 = 7;
    /// `SPA_VIDEO_TRANSFER_BT2020_12`.
    pub const BT2020_12: u32 = 11;
    /// `SPA_VIDEO_TRANSFER_BT2020_10`.
    pub const BT2020_10: u32 = 13;
    /// `SPA_VIDEO_TRANSFER_PQ`.
    pub const PQ: u32 = 14;
    /// `SPA_VIDEO_TRANSFER_HLG`.
    pub const HLG: u32 = 15;
}

/// The accepted formats, most preferred first.
///
/// `BGRx` leads because it is what Mutter and `KWin` produce natively — matching
/// it means the server hands over its own composited buffer rather than running
/// a conversion — and because an opaque screen has no alpha worth carrying.
pub const PREFERRED_FORMATS: [u32; 2] = [video_format::BGRX, video_format::RGBX];

/// Builds the `EnumFormat` parameter offered at `pw_stream_connect`.
///
/// The framerate is offered as a range from 0/1 to 60/1 rather than pinned.
/// Pinning it is a real failure mode: a compositor whose output is 144 Hz, or
/// whose screen-cast source is idle and therefore reporting 0/1, will simply not
/// intersect with `25/1` and the stream never reaches `Streaming`. A still
/// capture does not care how fast frames arrive; it wants the first one.
#[must_use]
pub fn enum_format_param() -> Vec<u8> {
    let formats: Vec<Scalar> = PREFERRED_FORMATS.iter().copied().map(Scalar::id).collect();

    let mut properties = vec![
        Property::scalar(key::MEDIA_TYPE, &Scalar::id(MEDIA_TYPE_VIDEO)),
        Property::scalar(key::MEDIA_SUBTYPE, &Scalar::id(MEDIA_SUBTYPE_RAW)),
    ];

    // `Choice::enumerated` only fails on a type-mismatched list, which these
    // literals cannot be; the `if let` keeps that impossibility from becoming a
    // panic in a capture path.
    if let Some(property) =
        Property::choice(key::VIDEO_FORMAT, &Choice::enumerated(formats.clone()))
    {
        properties.push(property);
    }

    if let Some(property) = Property::choice(
        key::VIDEO_SIZE,
        &Choice::range(
            Scalar::rectangle(1920, 1080),
            Scalar::rectangle(1, 1),
            Scalar::rectangle(MAX_DIMENSION, MAX_DIMENSION),
        ),
    ) {
        properties.push(property);
    }

    if let Some(property) = Property::choice(
        key::VIDEO_FRAMERATE,
        &Choice::range(
            Scalar::fraction(0, 1),
            Scalar::fraction(0, 1),
            Scalar::fraction(60, 1),
        ),
    ) {
        properties.push(property);
    }

    Object {
        object_type: OBJECT_FORMAT,
        id: param::ENUM_FORMAT,
        properties,
    }
    .encode()
}

/// Builds the `ParamBuffers` response sent after format fixation.
///
/// PipeWire requires consumers that negotiated no video modifier to declare
/// their usable memory types here. A flags choice allows the producer to choose
/// either mapped `MemFd` storage or a direct `MemPtr`, while excluding DMA-BUF.
#[must_use]
pub fn shared_memory_buffer_param() -> Vec<u8> {
    let memory_types = Choice::flags(Scalar::int(data_type::SHARED_MEMORY_MASK));
    let memory_types = Property::choice(buffer_key::DATA_TYPE, &memory_types)
        .expect("a one-value flags choice is always uniform");

    Object {
        object_type: OBJECT_PARAM_BUFFERS,
        id: param::BUFFERS,
        properties: vec![memory_types],
    }
    .encode()
}

/// Why a negotiated format could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// The parameter was not a `Format` object at all.
    NotAFormat,
    /// The stream is not raw video — an encoded stream, or audio.
    WrongMedia {
        /// The `SPA_MEDIA_TYPE_*` reported.
        media_type: u32,
        /// The `SPA_MEDIA_SUBTYPE_*` reported.
        media_subtype: u32,
    },
    /// The `spa_video_format` property was absent.
    MissingFormat,
    /// The `size` property was absent, or was zero in a dimension.
    MissingSize,
    /// The server returned dimensions outside the range the client offered.
    UnsupportedSize {
        /// Width the server selected.
        width: u32,
        /// Height the server selected.
        height: u32,
    },
    /// The server selected a DMA-BUF modifier even though none was offered.
    UnexpectedModifier,
    /// The server fixated on a format outside [`PREFERRED_FORMATS`].
    ///
    /// Should be unreachable — a server may only pick from what the client
    /// offered — but a buggy or hostile one is not a reason to index off the
    /// end of a buffer.
    UnsupportedFormat(u32),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAFormat => write!(f, "the stream parameter was not a Format object"),
            Self::WrongMedia {
                media_type,
                media_subtype,
            } => write!(
                f,
                "the stream carries media type {media_type}/{media_subtype}, not raw video \
                 ({MEDIA_TYPE_VIDEO}/{MEDIA_SUBTYPE_RAW})"
            ),
            Self::MissingFormat => write!(f, "the agreed format omitted the pixel format"),
            Self::MissingSize => write!(f, "the agreed format omitted a usable frame size"),
            Self::UnsupportedSize { width, height } => write!(
                f,
                "the server chose a {width}x{height} frame, outside the offered 1x1 to \
                 {MAX_DIMENSION}x{MAX_DIMENSION} range"
            ),
            Self::UnexpectedModifier => write!(
                f,
                "the server selected a DMA-BUF modifier even though the client offered only the \
                 modifier-less shared-memory path"
            ),
            Self::UnsupportedFormat(value) => write!(
                f,
                "the server chose spa_video_format {value}, which was never offered"
            ),
        }
    }
}

/// A format the server has fixated, reduced to what Scrozz needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// How the bytes are ordered once packed.
    pub pixel_format: PixelFormat,
    /// Whether the fourth byte is undefined padding that must be forced opaque.
    pub opaque_padding: bool,
    /// The colour space, as far as the server was willing to say.
    pub color_space: ColorSpace,
}

impl Negotiated {
    /// Bytes in one tightly-packed row.
    #[must_use]
    pub const fn packed_stride(&self) -> usize {
        self.width as usize * 4
    }

    /// Bytes in a tightly-packed frame.
    #[must_use]
    pub const fn packed_len(&self) -> usize {
        self.packed_stride() * self.height as usize
    }
}

/// Maps an `spa_video_format` to a Scrozz pixel format.
///
/// The boolean is whether the fourth byte is undefined padding. See the module
/// documentation for why that distinction is kept rather than collapsed.
#[must_use]
pub const fn pixel_format(spa_format: u32) -> Option<(PixelFormat, bool)> {
    match spa_format {
        video_format::BGRX => Some((PixelFormat::Bgra8, true)),
        video_format::RGBX => Some((PixelFormat::Rgba8, true)),
        _ => None,
    }
}

/// Maps exact SPA primaries/transfer pairs to a Scrozz colour space.
///
/// Anything unrecognised — including the common case of the property being
/// absent entirely — is [`ColorSpace::Unknown`]. Guessing sRGB would be right
/// most of the time and silently wrong on a wide-gamut monitor, and an encoder
/// that knows the space is unknown can decline to embed a profile instead of
/// embedding a false one.
#[must_use]
pub const fn color_space(primaries: Option<u32>, transfer: Option<u32>) -> ColorSpace {
    match (primaries, transfer) {
        (Some(primaries::BT709), Some(transfer::SRGB)) => ColorSpace::Srgb,
        (Some(primaries::SMPTE_EG432), Some(transfer::SRGB)) => ColorSpace::DisplayP3,
        (Some(primaries::BT2020), Some(transfer::BT2020_10 | transfer::BT2020_12)) => {
            ColorSpace::Rec2020
        }
        _ => ColorSpace::Unknown,
    }
}

/// Reads a fixated `Format` parameter.
///
/// # Errors
///
/// Returns [`FormatError`] when the parameter is not a raw-video format, omits
/// the pixel format or size, or names a format that was never offered.
pub fn parse_format(bytes: &[u8]) -> Result<Negotiated, FormatError> {
    let object = super::pod::ObjectRef::parse(bytes).ok_or(FormatError::NotAFormat)?;
    if object.object_type != OBJECT_FORMAT || object.id != param::FORMAT {
        return Err(FormatError::NotAFormat);
    }

    let media_type = object
        .property(key::MEDIA_TYPE)
        .and_then(|property| property.as_id())
        .unwrap_or(0);
    let media_subtype = object
        .property(key::MEDIA_SUBTYPE)
        .and_then(|property| property.as_id())
        .unwrap_or(0);
    if media_type != MEDIA_TYPE_VIDEO || media_subtype != MEDIA_SUBTYPE_RAW {
        return Err(FormatError::WrongMedia {
            media_type,
            media_subtype,
        });
    }

    let spa_format = object
        .property(key::VIDEO_FORMAT)
        .and_then(|property| property.as_id())
        .ok_or(FormatError::MissingFormat)?;
    let (pixel_format, opaque_padding) =
        pixel_format(spa_format).ok_or(FormatError::UnsupportedFormat(spa_format))?;

    if object.property(key::VIDEO_MODIFIER).is_some() {
        return Err(FormatError::UnexpectedModifier);
    }

    let (width, height) = object
        .property(key::VIDEO_SIZE)
        .and_then(|property| property.as_rectangle())
        .filter(|(width, height)| *width > 0 && *height > 0)
        .ok_or(FormatError::MissingSize)?;
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(FormatError::UnsupportedSize { width, height });
    }

    Ok(Negotiated {
        width,
        height,
        pixel_format,
        opaque_padding,
        color_space: color_space(
            object
                .property(key::VIDEO_PRIMARIES)
                .and_then(|property| property.as_id()),
            object
                .property(key::VIDEO_TRANSFER)
                .and_then(|property| property.as_id()),
        ),
    })
}

/// Why a buffer could not be turned into a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BufferError {
    /// Zero dimensions cannot describe a frame.
    InvalidDimensions {
        /// Width supplied by the caller.
        width: u32,
        /// Height supplied by the caller.
        height: u32,
    },
    /// The geometry cannot be represented safely as byte lengths.
    SizeOverflow {
        /// Width supplied by the caller.
        width: u32,
        /// Height supplied by the caller.
        height: u32,
    },
    /// The chunk reported a stride that cannot describe rows of this width.
    ///
    /// Includes zero and negative strides. SPA types `stride` as signed and a
    /// negative value means bottom-up rows, which no compositor produces for
    /// screen cast; guessing wrong would flip the image.
    BadStride {
        /// The stride the chunk reported.
        stride: i32,
        /// The minimum a row of this width needs.
        needed: usize,
    },
    /// The mapped memory is shorter than the geometry requires.
    Short {
        /// Bytes actually available after the chunk offset.
        available: usize,
        /// Bytes the declared geometry needs.
        needed: usize,
    },
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDimensions { width, height } => {
                write!(f, "a {width}x{height} buffer cannot describe a frame")
            }
            Self::SizeOverflow { width, height } => write!(
                f,
                "a {width}x{height} frame is too large to represent safely in memory"
            ),
            Self::BadStride { stride, needed } => write!(
                f,
                "the buffer reported a row stride of {stride} bytes, which cannot hold a row \
                 needing {needed}"
            ),
            Self::Short { available, needed } => write!(
                f,
                "the buffer holds {available} bytes but the agreed frame size needs {needed}"
            ),
        }
    }
}

/// Copies a strided buffer into tightly-packed rows.
///
/// PipeWire's row stride is whatever the producer found convenient — page
/// alignment, a GPU tiling requirement, a 64-byte cache line — and it is
/// routinely larger than `width * 4`. Copying `width * height * 4` bytes
/// straight out of such a buffer produces the classic diagonal-shear image, so
/// the stride is honoured row by row.
///
/// When `opaque_padding` is set, the fourth byte of each pixel is overwritten
/// with `0xFF`, because in an `x` format it is undefined rather than opaque.
///
/// # Errors
///
/// Returns [`BufferError`] for a stride too small for the width, or a buffer too
/// short for the declared geometry.
pub fn pack_rows(
    source: &[u8],
    stride: i32,
    width: u32,
    height: u32,
    opaque_padding: bool,
) -> Result<Vec<u8>, BufferError> {
    if width == 0 || height == 0 {
        return Err(BufferError::InvalidDimensions { width, height });
    }

    let dimensions_overflow = || BufferError::SizeOverflow { width, height };
    let row = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(dimensions_overflow)?;
    let output_len = row
        .checked_mul(usize::try_from(height).map_err(|_| dimensions_overflow())?)
        .filter(|len| *len <= isize::MAX as usize)
        .ok_or_else(dimensions_overflow)?;

    let stride_usize = usize::try_from(stride).unwrap_or(0);
    if stride_usize < row || stride_usize == 0 {
        return Err(BufferError::BadStride {
            stride,
            needed: row,
        });
    }

    // The final row needs only `row` bytes, not a whole stride: a producer is
    // entitled to end the mapping there, and demanding the padding would reject
    // a perfectly good buffer.
    let needed = stride_usize
        .checked_mul(height.saturating_sub(1) as usize)
        .and_then(|prefix| prefix.checked_add(row))
        .filter(|len| *len <= isize::MAX as usize)
        .ok_or_else(dimensions_overflow)?;
    if source.len() < needed {
        return Err(BufferError::Short {
            available: source.len(),
            needed,
        });
    }

    let mut out = Vec::with_capacity(output_len);
    for index in 0..height as usize {
        let start = index * stride_usize;
        out.extend_from_slice(&source[start..start + row]);
    }

    if opaque_padding {
        for pixel in out.as_chunks_mut::<4>().0 {
            pixel[3] = 0xFF;
        }
    }

    Ok(out)
}

/// Presents one SPA chunk as a linear byte sequence.
///
/// `spa_chunk.offset` is explicitly modulo the mapping size, and a chunk may
/// wrap from the end of the mapping back to its beginning. The common
/// contiguous case is borrowed without allocation; only a wrapped chunk is
/// copied.
#[must_use]
pub fn linear_chunk(mapping: &[u8], offset: u32, size: u32) -> Cow<'_, [u8]> {
    if mapping.is_empty() || size == 0 {
        return Cow::Borrowed(&mapping[..0]);
    }

    let size = (size as usize).min(mapping.len());
    let offset = offset as usize % mapping.len();
    let tail = mapping.len() - offset;
    if size <= tail {
        return Cow::Borrowed(&mapping[offset..offset + size]);
    }

    let mut linear = Vec::with_capacity(size);
    linear.extend_from_slice(&mapping[offset..]);
    linear.extend_from_slice(&mapping[..size - tail]);
    Cow::Owned(linear)
}
