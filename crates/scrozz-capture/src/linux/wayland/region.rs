//! Placing a user-drawn region inside a portal-supplied monitor stream.
//!
//! The ScreenCast portal has no concept of a sub-rectangle. It hands back whole
//! monitors, so a region capture is a monitor capture followed by a crop — and
//! the crop is where the arithmetic can quietly go wrong, which is why it lives
//! here as plain values and is tested on every platform.
//!
//! # Three coordinate spaces, and the one thing that reconciles them
//!
//! - Scrozz asks for a rectangle in the **global logical desktop**, because
//!   that is what the selection overlay draws in.
//! - The portal reports each stream's `position` and `size` in the
//!   **compositor's logical space** — the same space, offset to the monitor.
//! - PipeWire delivers **physical pixels**, and under scaling there are more of
//!   them than the logical size suggests.
//!
//! The tempting shortcut is to use the display's scale factor for the last
//! conversion. It is wrong often enough to matter: under fractional scaling the
//! compositor rounds the pixel size of an output, so a 1.25× scaled 1920-logical
//! monitor is not reliably 2400 pixels wide. Native `wl_output` and `xdg-output`
//! facts provide the nominal per-output scale, but the delivered frame remains
//! authoritative for the exact crop ratio.
//!
//! What is always true is that the frame and the portal's `size` describe the
//! *same* monitor. Dividing one by the other recovers the real ratio the
//! compositor used, whatever it was. That is the number this module uses.
//!
//! # When it cannot be done
//!
//! `position` and `size` are optional in the portal specification and genuinely
//! absent on some backends. Without them a region cannot be located, and the
//! honest response is to say so: returning the whole monitor instead would hand
//! back an image the user did not ask for and had no way to notice was wrong.

use scrozz_core::{Display, Frame, LogicalRect, PhysicalSize, ScaleFactor};

use super::portal::StreamInfo;

/// Why a requested region cannot be assigned to one exact display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionDisplayError {
    /// The rectangle has no finite, positive area.
    Empty,
    /// The rectangle misses every currently known display.
    Outside,
    /// The rectangle overlaps a display but extends beyond the connected
    /// desktop.
    PartlyOutside,
    /// The rectangle overlaps more than one display or is ambiguous between
    /// mirrored displays.
    SpansDisplays,
}

impl RegionDisplayError {
    /// Turns a pre-prompt placement refusal into the public error.
    #[must_use]
    pub fn into_error(self) -> scrozz_core::Error {
        use scrozz_core::Error;

        match self {
            Self::Empty => Error::InvalidRequest("the selected region has no finite area".into()),
            Self::Outside => {
                Error::InvalidRequest("the selected region is not on any connected display".into())
            }
            Self::PartlyOutside => Error::InvalidRequest(
                "the selected region extends beyond the connected display area".into(),
            ),
            Self::SpansDisplays => Error::Unsupported {
                what: "capturing a region that spans displays on Wayland".into(),
                why: "The ScreenCast portal grants one monitor stream for a region capture. \
                      Scrozz cannot combine only part of that stream with pixels from another \
                      monitor, so it refuses before opening the portal picker instead of returning \
                      a clamped, incomplete image. Select a region wholly inside one display."
                    .into(),
            },
        }
    }
}

/// Finds the one display that wholly contains `region`.
///
/// A region touching another display only at its exclusive edge still belongs
/// to one display. Any positive overlap with another display is a multi-stream
/// composition request and is refused before portal negotiation.
pub fn display_for_region(
    region: LogicalRect,
    displays: &[Display],
) -> Result<&Display, RegionDisplayError> {
    let Some(region_edges) = rect_edges(region) else {
        return Err(RegionDisplayError::Empty);
    };

    let mut containing = displays.iter().filter(|display| {
        rect_edges(display.bounds).is_some_and(|bounds| contains(bounds, region_edges))
    });
    let first = containing.next();
    if let Some(display) = first {
        if containing.next().is_some()
            || displays.iter().any(|candidate| {
                !std::ptr::eq(candidate, display)
                    && rect_edges(candidate.bounds)
                        .is_some_and(|bounds| overlaps(bounds, region_edges))
            })
        {
            return Err(RegionDisplayError::SpansDisplays);
        }
        return Ok(display);
    }

    let overlaps = displays
        .iter()
        .filter(|display| {
            rect_edges(display.bounds).is_some_and(|bounds| overlaps(bounds, region_edges))
        })
        .count();
    match overlaps {
        0 => Err(RegionDisplayError::Outside),
        1 => Err(RegionDisplayError::PartlyOutside),
        _ => Err(RegionDisplayError::SpansDisplays),
    }
}

/// Why a portal stream cannot be proven to be the requested display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMatchError {
    /// The portal omitted its optional position or size.
    NoGeometry,
    /// Native Wayland output facts and the portal do not expose a common integer
    /// coordinate space.
    UnmappableDisplay,
    /// The portal reported an empty stream.
    DegenerateStream,
    /// The stream geometry names a different display.
    DifferentDisplay,
}

impl StreamMatchError {
    /// Turns exact-display verification into a public error.
    #[must_use]
    pub fn into_error(
        self,
        compositor: &str,
        display: &Display,
        stream: &StreamInfo,
    ) -> scrozz_core::Error {
        use scrozz_core::Error;

        let exact_what = format!("capturing display {:?} exactly on Wayland", display.name);
        match self {
            Self::NoGeometry => Error::Unsupported {
                what: exact_what,
                why: format!(
                    "the portal on {compositor} omitted the selected monitor's optional position \
                     or size. ScreenCast cannot target a monitor by Scrozz display id, so without \
                     that geometry there is no trustworthy way to prove which display was granted. \
                     The session is closed without returning pixels and its restore token is not \
                     retained"
                ),
            },
            Self::UnmappableDisplay => Error::Unsupported {
                what: exact_what,
                why: format!(
                    "the native Wayland output registry and the portal on {compositor} did not \
                     expose this display in the same integer compositor coordinate space. Scrozz \
                     refuses to guess that two differently described outputs are the same target"
                ),
            },
            Self::DegenerateStream => Error::Platform(format!(
                "the portal on {compositor} reported a monitor stream with no area"
            )),
            Self::DifferentDisplay => Error::Unsupported {
                what: exact_what,
                why: format!(
                    "the requested display is at {:?} with size {:?}, but the portal granted a \
                     stream at {:?} with size {:?}. ScreenCast's picker cannot be constrained by \
                     display id, so Scrozz closes the mismatched session without returning pixels \
                     and discards the restore token",
                    display.bounds.origin, display.bounds.size, stream.position, stream.size
                ),
            },
        }
    }
}

/// Why native Wayland geometry cannot uniquely identify one portal monitor
/// stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayIdentityError {
    /// The compositor did not describe the display in integral coordinates.
    Unmappable,
    /// Another connected display has the same geometry, as with mirrored outputs.
    Ambiguous,
}

impl DisplayIdentityError {
    /// Turns a pre-prompt identity refusal into the public error.
    #[must_use]
    pub fn into_error(self, compositor: &str, display: &Display) -> scrozz_core::Error {
        use scrozz_core::Error;

        let what = format!("capturing display {:?} exactly on Wayland", display.name);
        let why = match self {
            Self::Unmappable => format!(
                "{compositor}'s native wl_output/xdg-output facts did not expose this display in \
                 the integral compositor coordinate space used by the portal. Scrozz refuses \
                 before opening the picker instead of rounding output geometry into an \
                 exact-target claim"
            ),
            Self::Ambiguous => format!(
                "another connected display has the same compositor geometry as {:?}, as happens \
                 with mirrored outputs. The portal reports geometry but no OS display id, so \
                 Scrozz cannot tell which mirrored display was selected and refuses before opening \
                 the picker",
                display.name
            ),
        };
        Error::Unsupported { what, why }
    }
}

/// Proves that one display has a unique portal-comparable geometry.
pub fn verify_display_identity(
    display: &Display,
    displays: &[Display],
) -> Result<(), DisplayIdentityError> {
    let expected = stream_geometry(display.bounds).ok_or(DisplayIdentityError::Unmappable)?;
    let matches = displays
        .iter()
        .filter(|candidate| stream_geometry(candidate.bounds) == Some(expected))
        .count();
    if matches == 1 {
        Ok(())
    } else {
        Err(DisplayIdentityError::Ambiguous)
    }
}

/// Whether a reusable session still names the exact same physical output.
///
/// Display ids are derived from compositor-provided output names, so a hotplug
/// can replace an output without changing the public id or geometry. The
/// connection-scoped `wl_output` global identity closes that gap.
#[must_use]
pub fn display_identity_unchanged<Identity: PartialEq>(
    initial: &Display,
    initial_output: &Identity,
    current: &Display,
    current_output: &Identity,
) -> bool {
    initial.id == current.id && initial.bounds == current.bounds && initial_output == current_output
}

/// Verifies that a portal monitor stream is the exact requested display.
pub fn verify_stream_matches_display(
    stream: &StreamInfo,
    display: &Display,
) -> Result<(), StreamMatchError> {
    let (Some(position), Some(size)) = (stream.position, stream.size) else {
        return Err(StreamMatchError::NoGeometry);
    };
    if size.0 <= 0 || size.1 <= 0 {
        return Err(StreamMatchError::DegenerateStream);
    }

    let expected = stream_geometry(display.bounds).ok_or(StreamMatchError::UnmappableDisplay)?;
    if (position, size) == expected {
        Ok(())
    } else {
        Err(StreamMatchError::DifferentDisplay)
    }
}

type RectEdges = (f64, f64, f64, f64);

fn rect_edges(rect: LogicalRect) -> Option<RectEdges> {
    let right = rect.origin.x + rect.size.width;
    let bottom = rect.origin.y + rect.size.height;
    let edges = (rect.origin.x, rect.origin.y, right, bottom);
    (edges.0.is_finite()
        && edges.1.is_finite()
        && edges.2.is_finite()
        && edges.3.is_finite()
        && edges.2 > edges.0
        && edges.3 > edges.1)
        .then_some(edges)
}

fn contains(outer: RectEdges, inner: RectEdges) -> bool {
    inner.0 >= outer.0 && inner.1 >= outer.1 && inner.2 <= outer.2 && inner.3 <= outer.3
}

fn overlaps(a: RectEdges, b: RectEdges) -> bool {
    a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
}

fn stream_geometry(bounds: LogicalRect) -> Option<((i32, i32), (i32, i32))> {
    fn exact_i32(value: f64) -> Option<i32> {
        (value.is_finite()
            && value.fract() == 0.0
            && value >= f64::from(i32::MIN)
            && value <= f64::from(i32::MAX))
        .then_some(value as i32)
    }

    let x = exact_i32(bounds.origin.x)?;
    let y = exact_i32(bounds.origin.y)?;
    let width = exact_i32(bounds.size.width)?;
    let height = exact_i32(bounds.size.height)?;
    (width > 0 && height > 0).then_some(((x, y), (width, height)))
}

/// A crop rectangle in frame-local pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CropRect {
    /// Left edge, in pixels from the frame's left.
    pub x: u32,
    /// Top edge, in pixels from the frame's top.
    pub y: u32,
    /// Width in pixels. Always non-zero.
    pub width: u32,
    /// Height in pixels. Always non-zero.
    pub height: u32,
}

/// Why a region could not be placed inside a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CropError {
    /// The portal reported no position or no size for the stream.
    NoGeometry,
    /// The portal reported a zero or negative size.
    DegenerateStream,
    /// The region does not overlap the captured monitor at all.
    Outside,
    /// The region overlaps the monitor but is not wholly inside it.
    PartialOverlap,
    /// The captured frame's declared geometry cannot be copied safely.
    UnusableFrame,
}

impl CropError {
    /// Turns the failure into the error a caller should see.
    #[must_use]
    pub fn into_error(self, compositor: &str) -> scrozz_core::Error {
        use scrozz_core::Error;

        match self {
            Self::NoGeometry => Error::Unsupported {
                what: "capturing a region on Wayland".into(),
                why: format!(
                    "the portal on {compositor} granted a screen-cast stream but reported neither \
                     its position nor its size, so there is no way to tell which part of the \
                     image the selected region corresponds to. Capture the whole display instead, \
                     or crop afterwards in the editor"
                ),
            },
            Self::DegenerateStream => Error::Platform(format!(
                "the portal on {compositor} reported a stream with no area, which leaves nothing \
                 to crop from"
            )),
            Self::Outside => scrozz_core::Error::InvalidRequest(
                "the selected region lies entirely outside the display that was captured".into(),
            ),
            Self::PartialOverlap => Error::Unsupported {
                what: "capturing a region that spans displays on Wayland".into(),
                why: "the selected region is only partly inside the monitor stream granted by the \
                      portal. Returning the intersection would silently produce a smaller image, \
                      and ScreenCast supplies no second stream to complete it"
                    .into(),
            },
            Self::UnusableFrame => Error::Platform(format!(
                "the portal on {compositor} delivered a frame whose dimensions, stride, buffer \
                 length, or required crop allocation are inconsistent; no pixels were returned"
            )),
        }
    }
}

/// Works out which pixels of `frame_size` correspond to `region`.
///
/// `frame_size` is the negotiated pixel size of the stream, not the portal's
/// logical size; the ratio between the two is the scale actually in force.
///
/// # Errors
///
/// [`CropError::NoGeometry`] when the portal supplied no placement,
/// [`CropError::DegenerateStream`] when it supplied an empty one, and
/// [`CropError::Outside`] when the region misses the monitor entirely, and
/// [`CropError::PartialOverlap`] when returning it would require another stream.
pub fn plan_crop(
    region: LogicalRect,
    stream: &StreamInfo,
    frame_size: (u32, u32),
) -> Result<CropRect, CropError> {
    let (Some((origin_x, origin_y)), Some((logical_w, logical_h))) = (stream.position, stream.size)
    else {
        return Err(CropError::NoGeometry);
    };

    if logical_w <= 0 || logical_h <= 0 || frame_size.0 == 0 || frame_size.1 == 0 {
        return Err(CropError::DegenerateStream);
    }

    let stream_left = f64::from(origin_x);
    let stream_top = f64::from(origin_y);
    let stream_right = stream_left + f64::from(logical_w);
    let stream_bottom = stream_top + f64::from(logical_h);
    let Some((region_left, region_top, region_right, region_bottom)) = rect_edges(region) else {
        return Err(CropError::Outside);
    };
    let stream_edges = (stream_left, stream_top, stream_right, stream_bottom);
    let region_edges = (region_left, region_top, region_right, region_bottom);
    if !overlaps(stream_edges, region_edges) {
        return Err(CropError::Outside);
    }
    if !contains(stream_edges, region_edges) {
        return Err(CropError::PartialOverlap);
    }

    let scale_x = f64::from(frame_size.0) / f64::from(logical_w);
    let scale_y = f64::from(frame_size.1) / f64::from(logical_h);

    // Local logical coordinates, then pixels. Outward rounding: a region is a
    // selection, and losing a row of the thing the user framed is more annoying
    // than gaining one of the background.
    let left = ((region_left - stream_left) * scale_x).floor().max(0.0);
    let top = ((region_top - stream_top) * scale_y).floor().max(0.0);
    let right = ((region_right - stream_left) * scale_x)
        .ceil()
        .min(f64::from(frame_size.0));
    let bottom = ((region_bottom - stream_top) * scale_y)
        .ceil()
        .min(f64::from(frame_size.1));

    if !(left.is_finite() && top.is_finite() && right.is_finite() && bottom.is_finite()) {
        return Err(CropError::Outside);
    }

    if right <= left || bottom <= top {
        return Err(CropError::PartialOverlap);
    }

    Ok(CropRect {
        x: left as u32,
        y: top as u32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

/// Copies `rect` out of `frame` into a new tightly-packed frame.
///
/// Returns [`CropError::Outside`] if the rectangle does not lie wholly within
/// the frame and [`CropError::UnusableFrame`] if the frame's own geometry or
/// allocation cannot be represented safely.
///
/// # Errors
///
/// [`CropError::Outside`] when `rect` escapes the frame, or
/// [`CropError::UnusableFrame`] when the frame's buffer is shorter than its
/// declared geometry or the packed result cannot be allocated.
pub fn crop(frame: &Frame, rect: CropRect) -> Result<Frame, CropError> {
    let bpp = frame.format.bytes_per_pixel();
    let width = frame.width();
    let height = frame.height();

    let Some(right) = rect.x.checked_add(rect.width) else {
        return Err(CropError::Outside);
    };
    let Some(bottom) = rect.y.checked_add(rect.height) else {
        return Err(CropError::Outside);
    };
    if rect.width == 0 || rect.height == 0 || right > width || bottom > height {
        return Err(CropError::Outside);
    }

    let width = usize::try_from(width).map_err(|_| CropError::UnusableFrame)?;
    let height = usize::try_from(height).map_err(|_| CropError::UnusableFrame)?;
    let minimum_stride = width.checked_mul(bpp).ok_or(CropError::UnusableFrame)?;
    let source_len = frame
        .stride
        .checked_mul(height)
        .ok_or(CropError::UnusableFrame)?;
    if frame.stride < minimum_stride || frame.data.len() < source_len {
        return Err(CropError::UnusableFrame);
    }

    let crop_x = usize::try_from(rect.x).map_err(|_| CropError::UnusableFrame)?;
    let crop_y = usize::try_from(rect.y).map_err(|_| CropError::UnusableFrame)?;
    let crop_width = usize::try_from(rect.width).map_err(|_| CropError::UnusableFrame)?;
    let crop_height = usize::try_from(rect.height).map_err(|_| CropError::UnusableFrame)?;
    let out_stride = crop_width
        .checked_mul(bpp)
        .ok_or(CropError::UnusableFrame)?;
    let out_len = out_stride
        .checked_mul(crop_height)
        .filter(|len| *len <= super::pipewire::format::MAX_FRAME_BYTES)
        .ok_or(CropError::UnusableFrame)?;
    let x_bytes = crop_x.checked_mul(bpp).ok_or(CropError::UnusableFrame)?;
    let mut data = Vec::new();
    data.try_reserve_exact(out_len)
        .map_err(|_| CropError::UnusableFrame)?;

    for row in 0..crop_height {
        let source_row = crop_y.checked_add(row).ok_or(CropError::UnusableFrame)?;
        let start = source_row
            .checked_mul(frame.stride)
            .and_then(|offset| offset.checked_add(x_bytes))
            .ok_or(CropError::UnusableFrame)?;
        let end = start
            .checked_add(out_stride)
            .ok_or(CropError::UnusableFrame)?;
        let Some(slice) = frame.data.get(start..end) else {
            return Err(CropError::UnusableFrame);
        };
        data.extend_from_slice(slice);
    }
    debug_assert_eq!(data.len(), out_len);

    Ok(Frame {
        data,
        size: PhysicalSize::new(f64::from(rect.width), f64::from(rect.height)),
        stride: out_stride,
        format: frame.format,
        color_space: frame.color_space,
        scale: frame.scale,
    })
}

/// Recovers the scale factor the compositor actually applied to a stream.
///
/// PipeWire reports pixels and the portal reports logical units, so their ratio
/// *is* the scale — no guessing, no rounding assumptions, and correct under
/// fractional scaling where the display's advertised scale is not.
///
/// `fallback` is used when the portal supplied no size, which is the only case
/// where there is nothing to divide.
#[must_use]
pub fn resolve_scale(
    stream: &StreamInfo,
    frame_size: (u32, u32),
    fallback: ScaleFactor,
) -> ScaleFactor {
    let Some((logical_w, logical_h)) = stream.size else {
        return fallback;
    };
    if logical_w <= 0 || logical_h <= 0 || frame_size.0 == 0 || frame_size.1 == 0 {
        return fallback;
    }

    // Horizontal and vertical scales are equal on every compositor in practice;
    // averaging rather than picking one keeps a rounded-by-one output from
    // biasing the result in a single axis.
    let ratio = (f64::from(frame_size.0) / f64::from(logical_w)
        + f64::from(frame_size.1) / f64::from(logical_h))
        / 2.0;

    if ratio.is_finite() && ratio > 0.0 {
        ScaleFactor::new(ratio)
    } else {
        fallback
    }
}
