//! Stride-aware pixel buffer arithmetic.
//!
//! Free of every `windows` crate type, so `tests/windows.rs` can
//! `#[path]`-include it and test it on any platform.
//!
//! # Stride, and why it gets its own module
//!
//! A D3D11 staging texture mapped for read reports a `RowPitch` that is very
//! nearly always larger than `width * 4` — the driver aligns rows to 64, 128 or
//! 256 bytes, and for a 1366-pixel-wide window the padding is not subtle.
//! Copying `width * height * 4` bytes straight out of the mapped pointer
//! produces the classic diagonally-sheared screenshot, sliding a few pixels
//! further left on every row.
//!
//! Scrozz's [`scrozz_core::Frame`] carries `stride` precisely so the padding
//! never has to be removed. The functions here therefore **preserve** stride
//! wherever they can and only repack when the operation genuinely demands it
//! (cropping horizontally, compositing).

use super::geom::DeviceRect;

/// Bytes per pixel for every format this backend produces.
pub const BGRA_BYTES_PER_PIXEL: usize = 4;

/// Bytes needed to hold `height` rows of `stride` bytes.
#[must_use]
pub fn buffer_len(stride: usize, height: u32) -> usize {
    stride.saturating_mul(height as usize)
}

/// The tightest legal stride for a row of `width` BGRA pixels.
#[must_use]
pub fn min_stride(width: u32) -> usize {
    (width as usize).saturating_mul(BGRA_BYTES_PER_PIXEL)
}

/// Copies `height` rows out of a mapped texture, keeping the source stride.
///
/// This is the fast path and the one taken for a whole-display or whole-window
/// capture: no repacking, no per-pixel work, one `memcpy` per row. The returned
/// buffer is `src_stride * height` bytes and is meant to be paired with
/// `Frame { stride: src_stride, .. }`.
///
/// `src` may be longer than needed — a mapped subresource's `DepthPitch`
/// generally exceeds `RowPitch * height` — and rows beyond its end are simply
/// not produced, so a short or lying `RowPitch` truncates instead of panicking.
#[must_use]
pub fn copy_rows_keeping_stride(src: &[u8], src_stride: usize, height: u32) -> Vec<u8> {
    let rows = height as usize;
    let mut out = Vec::with_capacity(buffer_len(src_stride, height));
    for row in 0..rows {
        let start = row.saturating_mul(src_stride);
        let end = start.saturating_add(src_stride);
        if end > src.len() {
            out.resize(buffer_len(src_stride, height), 0);
            break;
        }
        out.extend_from_slice(&src[start..end]);
    }
    out
}

/// Extracts `crop` — expressed relative to the source buffer's top-left — into
/// a tightly packed buffer.
///
/// Repacking is unavoidable here: a horizontal crop changes where every row
/// begins. Returns the buffer and its stride, which is `crop.width() * 4`.
/// A crop outside the source is clamped; a crop with no overlap yields an empty
/// buffer, which the caller must reject rather than hand downstream.
#[must_use]
pub fn crop(
    src: &[u8],
    src_stride: usize,
    src_width: u32,
    src_height: u32,
    crop: DeviceRect,
) -> (Vec<u8>, usize, u32, u32) {
    let bounds = DeviceRect::new(0, 0, src_width as i32, src_height as i32);
    let Some(area) = crop.intersection(bounds) else {
        return (Vec::new(), 0, 0, 0);
    };

    let w = area.width() as u32;
    let h = area.height() as u32;
    let dst_stride = min_stride(w);
    let mut out = vec![0u8; buffer_len(dst_stride, h)];

    for row in 0..h as usize {
        let sy = area.top as usize + row;
        let sx = area.left as usize * BGRA_BYTES_PER_PIXEL;
        let s0 = sy * src_stride + sx;
        let s1 = s0 + dst_stride;
        if s1 > src.len() {
            break;
        }
        let d0 = row * dst_stride;
        out[d0..d0 + dst_stride].copy_from_slice(&src[s0..s1]);
    }

    (out, dst_stride, w, h)
}

/// Forces every alpha byte to 255.
///
/// GDI `BitBlt` leaves the alpha channel of a 32-bit DIB undefined — in
/// practice mostly zero — so a PNG encoded straight from a `BitBlt` capture is
/// fully transparent. WGC needs no such fixup; this exists solely for the
/// fallback path, and calling it on WGC output would destroy a window's real
/// rounded-corner alpha.
pub fn force_opaque_alpha(buf: &mut [u8], stride: usize, width: u32, height: u32) {
    for row in 0..height as usize {
        let base = row * stride;
        for px in 0..width as usize {
            let i = base + px * BGRA_BYTES_PER_PIXEL + 3;
            if i < buf.len() {
                buf[i] = 0xFF;
            }
        }
    }
}

/// Whether any pixel has a non-zero alpha byte.
///
/// Used to decide whether a `PrintWindow` result is worth keeping: a window
/// with no redirection surface comes back entirely transparent black, which is
/// worth reporting as a failure rather than saving.
#[must_use]
pub fn has_any_alpha(buf: &[u8], stride: usize, width: u32, height: u32) -> bool {
    for row in 0..height as usize {
        let base = row * stride;
        for px in 0..width as usize {
            let i = base + px * BGRA_BYTES_PER_PIXEL + 3;
            if i < buf.len() && buf[i] != 0 {
                return true;
            }
        }
    }
    false
}

/// A writable BGRA image plus the two numbers needed to index it.
///
/// Stride is carried separately from width because on the D3D11 path they are
/// almost never equal — see [`copy_rows_keeping_stride`]. Bundling them into
/// one value means a caller cannot pass the width where the stride belongs,
/// which is the single most common way to produce the classic skewed image.
pub struct Plane<'a> {
    /// The pixel bytes.
    pub data: &'a mut [u8],
    /// Bytes per row, at least `width * 4`.
    pub stride: usize,
    /// Pixels per row.
    pub width: u32,
    /// Number of rows.
    pub height: u32,
}

/// The read-only counterpart of [`Plane`].
pub struct PlaneRef<'a> {
    /// The pixel bytes.
    pub data: &'a [u8],
    /// Bytes per row, at least `width * 4`.
    pub stride: usize,
    /// Pixels per row.
    pub width: u32,
    /// Number of rows.
    pub height: u32,
}

/// Nearest-neighbour blit of one display's pixels into a composite canvas.
///
/// All-displays capture on a mixed-DPI desktop has no single pixel grid: a 150%
/// monitor beside a 100% one cannot be laid side by side without one of them
/// being resampled. Rather than refuse (which loses a genuinely useful feature)
/// or silently pick one scale (which shrinks the sharper monitor), the composite
/// is built at the *largest* scale present and lower-DPI monitors are scaled up.
/// Nearest neighbour is deliberate: it is exact when the ratio is 1.0, which is
/// the overwhelmingly common single-scale case, and never introduces colours
/// that were not in the capture.
///
/// `dst_rect` is in destination pixels and may be partly off-canvas; the blit
/// clips.
pub fn blit_nearest(dst: &mut Plane<'_>, dst_rect: DeviceRect, src: &PlaneRef<'_>) {
    let (dst_stride, dst_width, dst_height) = (dst.stride, dst.width, dst.height);
    let (src_stride, src_width, src_height) = (src.stride, src.width, src.height);
    if dst_rect.is_empty() || src_width == 0 || src_height == 0 {
        return;
    }
    let canvas = DeviceRect::new(0, 0, dst_width as i32, dst_height as i32);
    let Some(clipped) = dst_rect.intersection(canvas) else {
        return;
    };

    let dw = f64::from(dst_rect.width());
    let dh = f64::from(dst_rect.height());

    for y in clipped.top..clipped.bottom {
        let ty = f64::from(y - dst_rect.top) / dh;
        let sy = ((ty * f64::from(src_height)) as usize).min(src_height as usize - 1);
        let src_row = sy * src_stride;
        let dst_row = y as usize * dst_stride;

        for x in clipped.left..clipped.right {
            let tx = f64::from(x - dst_rect.left) / dw;
            let sx = ((tx * f64::from(src_width)) as usize).min(src_width as usize - 1);
            let s = src_row + sx * BGRA_BYTES_PER_PIXEL;
            let d = dst_row + x as usize * BGRA_BYTES_PER_PIXEL;
            if s + BGRA_BYTES_PER_PIXEL <= src.data.len()
                && d + BGRA_BYTES_PER_PIXEL <= dst.data.len()
            {
                dst.data[d..d + BGRA_BYTES_PER_PIXEL]
                    .copy_from_slice(&src.data[s..s + BGRA_BYTES_PER_PIXEL]);
            }
        }
    }
}

/// Flips a bottom-up DIB into top-down order in place.
///
/// `CreateDIBSection` is asked for a negative height so this is normally
/// unnecessary, but `GetDIBits` on some drivers ignores that and hands back
/// bottom-up rows regardless.
pub fn flip_vertical(buf: &mut [u8], stride: usize, height: u32) {
    let rows = height as usize;
    if rows < 2 || stride == 0 || buf.len() < rows * stride {
        return;
    }
    for row in 0..rows / 2 {
        let top = row * stride;
        let bottom = (rows - 1 - row) * stride;
        let (a, b) = buf.split_at_mut(bottom);
        a[top..top + stride].swap_with_slice(&mut b[..stride]);
    }
}
