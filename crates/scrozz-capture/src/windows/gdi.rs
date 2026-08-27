//! `BitBlt`/`PrintWindow` — the fallback path.
//!
//! Used when [`super::wgc`] cannot run: Windows 10 builds older than 1903, and
//! machines where no Direct3D 11 device can be created at all.
//!
//! # What this path cannot do
//!
//! These are not bugs to be fixed later; they are properties of scraping the
//! screen instead of reading the composition tree, and they are the reason WGC
//! exists.
//!
//! - **No per-window compositing.** `BitBlt` from the screen DC copies whatever
//!   pixels are on the screen, so anything overlapping the target window is
//!   captured too. `PrintWindow` avoids that by asking the window to redraw
//!   itself, but see below.
//! - **Hardware-accelerated content comes back black.** A window whose content
//!   is drawn by D3D, a video overlay, or a browser with GPU compositing on has
//!   no GDI representation to copy. `PW_RENDERFULLCONTENT` fixes this for many
//!   such windows on Windows 8.1+ by going through DWM, but not for windows
//!   with `WS_EX_NOREDIRECTIONBITMAP` — which is most modern UWP and WinUI
//!   applications — because there is no redirection surface to read.
//! - **No alpha.** `BitBlt` leaves the alpha channel of a 32-bit DIB
//!   undefined, so it is forced opaque. A window's rounded corners and drop
//!   shadow are therefore square and solid here, where WGC would give the true
//!   shape.
//! - **Occlusion is real.** Whatever is on top of the screen at the moment of
//!   the call is in the capture, including this application's own overlay if it
//!   has not been hidden first.
//! - **The cursor is never included**, because `BitBlt` reads the frame buffer
//!   rather than the composed image. Drawing it would mean compositing an
//!   icon by hand, which is exactly the kind of synthetic touch-up decision D9
//!   rules out for window captures; [`CursorMode::Visible`] is therefore not
//!   honoured on this path.
//!
//! [`CursorMode::Visible`]: scrozz_core::CursorMode

use scrozz_core::{ColorSpace, Error, Frame, PixelFormat, Result, ScaleFactor, Size};
use windows::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC,
        CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP, HDC, HGDIOBJ,
        ReleaseDC, SRCCOPY, SelectObject,
    },
};

use super::{ffi, geom::DeviceRect, pixels};

/// An owned screen DC, released on drop.
///
/// Leaking a screen DC exhausts a per-session GDI quota and eventually breaks
/// drawing for the whole desktop, so every early return has to release it —
/// which is what this exists to guarantee.
struct ScreenDc(HDC);

impl ScreenDc {
    fn get() -> Result<Self> {
        let hdc = unsafe { GetDC(None) };
        if hdc.is_invalid() {
            return Err(Error::Platform("GetDC(NULL) failed".into()));
        }
        Ok(Self(hdc))
    }
}

impl Drop for ScreenDc {
    fn drop(&mut self) {
        unsafe { ReleaseDC(None, self.0) };
    }
}

/// An owned memory DC plus its DIB section.
struct MemoryDib {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut core::ffi::c_void,
    width: u32,
    height: u32,
}

impl MemoryDib {
    /// Creates a top-down 32-bit BGRA DIB.
    fn create(reference: HDC, width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::Platform("zero-sized capture".into()));
        }

        let dc = unsafe { CreateCompatibleDC(Some(reference)) };
        if dc.is_invalid() {
            return Err(Error::Platform("CreateCompatibleDC failed".into()));
        }

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or(40),
                biWidth: i32::try_from(width).unwrap_or(i32::MAX),
                // Negative height means top-down rows, which is the order every
                // consumer of `Frame` expects. A bottom-up DIB would need a flip
                // pass over the whole image for no reason.
                biHeight: -i32::try_from(height).unwrap_or(i32::MAX),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits = core::ptr::null_mut();
        let bitmap =
            unsafe { CreateDIBSection(Some(dc), &raw const info, DIB_RGB_COLORS, &mut bits, None, 0) };
        let bitmap = match bitmap {
            Ok(b) if !b.is_invalid() && !bits.is_null() => b,
            _ => {
                unsafe { let _ = DeleteDC(dc); };
                return Err(Error::Platform("CreateDIBSection failed".into()));
            }
        };

        let previous = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
        Ok(Self {
            dc,
            bitmap,
            previous,
            bits,
            width,
            height,
        })
    }

    /// Stride of the DIB.
    ///
    /// A 32-bit DIB is exactly four bytes per pixel with no row padding, since
    /// `width * 4` is already a multiple of the four-byte alignment
    /// `CreateDIBSection` guarantees. Unlike the D3D11 path there is nothing to
    /// discover here — but the value still travels with the frame so callers
    /// never have to know which path produced it.
    const fn stride(&self) -> usize {
        (self.width as usize) * pixels::BGRA_BYTES_PER_PIXEL
    }

    fn to_bytes(&self) -> Vec<u8> {
        let len = pixels::buffer_len(self.stride(), self.height);
        let src = unsafe { core::slice::from_raw_parts(self.bits.cast::<u8>(), len) };
        src.to_vec()
    }
}

impl Drop for MemoryDib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

/// Captures a rectangle of the virtual desktop.
///
/// `rect` is in virtual-desktop device pixels and may have a negative origin,
/// which is normal for a monitor placed left of or above the primary.
///
/// # Errors
///
/// Returns [`Error::Platform`] when GDI refuses any step of the copy.
pub fn capture_rect(rect: DeviceRect, scale: ScaleFactor) -> Result<Frame> {
    if rect.is_empty() {
        return Err(Error::Platform("empty capture rectangle".into()));
    }

    let screen = ScreenDc::get()?;
    let width = rect.width() as u32;
    let height = rect.height() as u32;
    let dib = MemoryDib::create(screen.0, width, height)?;

    unsafe {
        BitBlt(
            dib.dc,
            0,
            0,
            rect.width(),
            rect.height(),
            Some(screen.0),
            rect.left,
            rect.top,
            // `CAPTUREBLT` includes layered windows, which are otherwise
            // simply absent from the result — tooltips, menus and most
            // notification popups are layered.
            SRCCOPY | CAPTUREBLT,
        )
    }
    .map_err(|e| Error::Platform(format!("BitBlt failed: {e}")))?;

    let mut data = dib.to_bytes();
    let stride = dib.stride();
    pixels::force_opaque_alpha(&mut data, stride, width, height);

    Ok(Frame {
        data,
        size: Size::new(f64::from(width), f64::from(height)),
        stride,
        format: PixelFormat::Bgra8,
        color_space: ColorSpace::Srgb,
        scale,
    })
}

/// Captures a single window by asking it to render itself.
///
/// # Errors
///
/// Returns [`Error::TargetGone`] if the window will not render, which for this
/// path also covers "the window has no redirection surface" — an all-black or
/// all-transparent result is reported as a failure rather than saved, because
/// silently handing back an empty image is worse than saying it did not work.
pub fn capture_window(hwnd: HWND, bounds: DeviceRect, scale: ScaleFactor) -> Result<Frame> {
    if bounds.is_empty() {
        return Err(Error::TargetGone("window has no on-screen area".into()));
    }

    let screen = ScreenDc::get()?;
    let width = bounds.width() as u32;
    let height = bounds.height() as u32;
    let dib = MemoryDib::create(screen.0, width, height)?;

    // `PW_RENDERFULLCONTENT` routes through DWM instead of sending `WM_PRINT`,
    // which is the only way this captures hardware-accelerated child content.
    // Windows 8.1+; on older builds the flag is ignored and the call degrades
    // to a plain `WM_PRINT`, which is still better than nothing.
    let ok = unsafe { ffi::PrintWindow(hwnd, dib.dc, ffi::PW_RENDERFULLCONTENT) };
    if !ok.as_bool() {
        return Err(Error::TargetGone(
            "PrintWindow failed; the window has probably closed".into(),
        ));
    }

    let mut data = dib.to_bytes();
    let stride = dib.stride();

    // A window with no redirection surface renders nothing at all and the DIB
    // is returned untouched — every byte zero. That is indistinguishable from
    // a real capture of a fully transparent window, which does not exist, so
    // treating it as a failure is safe and far more useful than saving it.
    if data.iter().all(|&b| b == 0) {
        return Err(Error::Unsupported {
            what: "capturing this window without Windows.Graphics.Capture".into(),
            why: "it has no GDI-readable surface (WS_EX_NOREDIRECTIONBITMAP); \
                  Windows 10 1903 or newer is required to capture it".into(),
        });
    }

    pixels::force_opaque_alpha(&mut data, stride, width, height);

    Ok(Frame {
        data,
        size: Size::new(f64::from(width), f64::from(height)),
        stride,
        format: PixelFormat::Bgra8,
        color_space: ColorSpace::Srgb,
        scale,
    })
}
