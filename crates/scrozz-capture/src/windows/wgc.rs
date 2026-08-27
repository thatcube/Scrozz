//! `Windows.Graphics.Capture` — the primary path.
//!
//! WGC asks DWM for the composed surface of a window or monitor. That is what
//! makes it correct where `BitBlt` is not: it captures hardware-accelerated
//! content, layered and per-pixel-alpha windows, and windows that are partly
//! occluded or off-screen, because it reads the composition surface rather than
//! scraping the screen.
//!
//! # Version floor
//!
//! - **Windows 10 1803 (build 17134)** — WGC exists.
//! - **Windows 10 1903 (build 18362)** — `CreateFreeThreaded`, which is what
//!   lets a still capture run without pumping a message loop or owning a
//!   `DispatcherQueue`. This backend requires it, so 1903 is the real floor.
//! - **Windows 10 2004 (build 19041)** — `IsCursorCaptureEnabled`, so the
//!   cursor can be excluded. On earlier builds the cursor is always drawn and
//!   [`CursorMode::Hidden`] cannot be honoured.
//! - **Windows 11 (build 22000)** — `IsBorderRequired`. Before this, WGC draws
//!   a **yellow border** around the captured content and there is no API to
//!   turn it off; the property simply does not exist on the session object and
//!   the `QueryInterface` for it fails. That is why the call below tolerates
//!   failure instead of treating it as an error: on Windows 10 the border is a
//!   fact of the platform, and refusing to capture would be worse than
//!   capturing with it.
//!
//! [`CursorMode::Hidden`]: scrozz_core::CursorMode

use scrozz_core::{ColorSpace, CursorMode, Error, Frame, PixelFormat, Result, ScaleFactor, Size};
use windows::Graphics::DirectX::{
    Direct3D11::{IDirect3DDevice, IDirect3DSurface},
    DirectXPixelFormat,
};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::System::WinRT::{
    Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
    Graphics::Capture::IGraphicsCaptureItemInterop,
};
use windows::{
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        SizeInt32,
    },
    Win32::{
        Foundation::HWND,
        Graphics::{
            Direct3D11::{
                D3D11_BOX, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, ID3D11Device,
                ID3D11DeviceContext, ID3D11Texture2D,
            },
            Dxgi::IDXGIDevice,
            Gdi::HMONITOR,
        },
    },
    core::Interface,
};

use super::pixels;

/// A live D3D11 device, kept for the lifetime of the backend.
///
/// Device creation is the expensive part of a WGC capture — tens of
/// milliseconds — and repeating it per screenshot would be plainly visible.
pub struct WgcDevice {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// The WinRT device wrapper the frame pool needs.
    ///
    /// The same underlying D3D11 device as `device`, seen through the WinRT
    /// projection that `Direct3D11CaptureFramePool` requires. Keeping both
    /// avoids re-wrapping on every capture.
    winrt_device: IDirect3DDevice,
}

// The D3D11 device is created with the default (thread-safe) flags and every
// use below goes through the immediate context under `&self`, so sharing it
// across threads is sound. WGC's free-threaded frame pool has the same
// expectation.
unsafe impl Send for WgcDevice {}
unsafe impl Sync for WgcDevice {}

impl WgcDevice {
    /// Creates the device, preferring hardware and falling back to WARP.
    ///
    /// The WARP fallback matters more than it looks: a Remote Desktop session,
    /// a VM with no GPU passthrough, and a machine whose display driver has
    /// just crashed all fail to produce a hardware device, and in every one of
    /// those cases the user still wants a screenshot.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when neither driver type yields a device,
    /// which means this machine cannot run the WGC path at all.
    pub fn new() -> Result<Self> {
        let device = create_d3d11_device()?;
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|e| Error::Platform(format!("GetImmediateContext failed: {e}")))?;

        let dxgi: IDXGIDevice = device
            .cast()
            .map_err(|e| Error::Platform(format!("device is not an IDXGIDevice: {e}")))?;

        let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .map_err(|e| {
                Error::Platform(format!("CreateDirect3D11DeviceFromDXGIDevice failed: {e}"))
            })?
            .cast()
            .map_err(|e| Error::Platform(format!("not an IDirect3DDevice: {e}")))?;

        Ok(Self {
            device,
            context,
            winrt_device,
        })
    }
}

fn create_d3d11_device() -> Result<ID3D11Device> {
    const DRIVERS: [D3D_DRIVER_TYPE; 2] = [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP];

    for driver in DRIVERS {
        let mut device: Option<ID3D11Device> = None;
        let created = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                // BGRA support is not optional: WGC interop refuses a device
                // without it.
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                None,
            )
        };
        if created.is_ok()
            && let Some(device) = device
        {
            return Ok(device);
        }
    }
    Err(Error::Unsupported {
        what: "Windows.Graphics.Capture".into(),
        why: "no Direct3D 11 device could be created, not even a WARP software one".into(),
    })
}

/// Whether this machine can run the WGC path at all.
#[must_use]
pub fn is_supported() -> bool {
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

/// The interop factory that turns an `HWND` or `HMONITOR` into a capture item.
fn interop() -> Result<IGraphicsCaptureItemInterop> {
    windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(|e| {
        Error::Unsupported {
            what: "Windows.Graphics.Capture".into(),
            why: format!("its activation factory is unavailable on this build: {e}"),
        }
    })
}

/// A capture item for a window.
///
/// # Errors
///
/// Returns [`Error::TargetGone`] if the window has closed, and
/// [`Error::Unsupported`] if this build cannot capture it — a few system
/// windows are permanently off-limits to WGC.
pub fn item_for_window(hwnd: HWND) -> Result<GraphicsCaptureItem> {
    unsafe { interop()?.CreateForWindow::<GraphicsCaptureItem>(hwnd) }.map_err(|e| {
        if e.code() == windows::Win32::Foundation::E_INVALIDARG {
            Error::TargetGone("window has closed".into())
        } else {
            Error::Platform(format!("CreateForWindow failed: {e}"))
        }
    })
}

/// A capture item for a monitor.
///
/// # Errors
///
/// Returns [`Error::TargetGone`] if the monitor has been disconnected.
pub fn item_for_monitor(monitor: HMONITOR) -> Result<GraphicsCaptureItem> {
    unsafe { interop()?.CreateForMonitor::<GraphicsCaptureItem>(monitor) }.map_err(|e| {
        if e.code() == windows::Win32::Foundation::E_INVALIDARG {
            Error::TargetGone("display has been disconnected".into())
        } else {
            Error::Platform(format!("CreateForMonitor failed: {e}"))
        }
    })
}

/// How long to wait for the first frame before giving up.
///
/// WGC delivers a frame on the next composition pass. At 60 Hz that is under
/// 17 ms; a full second is generous enough for a stalled compositor or a
/// heavily loaded machine, and short enough that a hung capture does not look
/// like a hung application.
const FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);

/// How long to sleep between polls while waiting for that frame.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2);

/// Captures a single frame from a capture item.
///
/// # Errors
///
/// Returns [`Error::TargetGone`] if the target closes mid-capture, and
/// [`Error::Platform`] if the compositor never delivers a frame.
pub fn capture_item(
    device: &WgcDevice,
    item: &GraphicsCaptureItem,
    cursor: CursorMode,
    scale: ScaleFactor,
) -> Result<Frame> {
    let size = item
        .Size()
        .map_err(|_| Error::TargetGone("capture target closed before it could be sized".into()))?;
    if size.Width <= 0 || size.Height <= 0 {
        return Err(Error::TargetGone("capture target has no area".into()));
    }

    let pool = create_pool(device, size)?;
    let session = pool
        .CreateCaptureSession(item)
        .map_err(|e| Error::Platform(format!("CreateCaptureSession failed: {e}")))?;

    // Windows 10 2004+. On older builds the cursor is always included and
    // there is nothing to be done about it, so a failure here is not fatal.
    let _ = session.SetIsCursorCaptureEnabled(matches!(cursor, CursorMode::Visible));

    // Windows 11 only. Before that WGC paints a yellow border around the
    // captured region and the property does not exist; see the module docs.
    let _ = session.SetIsBorderRequired(false);

    session
        .StartCapture()
        .map_err(|e| Error::Platform(format!("StartCapture failed: {e}")))?;

    let result = poll_frame(device, &pool);

    // Always tear down, even on the error path: a live session keeps the
    // capture-in-progress indicator on screen and pins the frame pool's
    // textures.
    let _ = session.Close();
    let _ = pool.Close();

    let (data, stride, width, height) = result?;
    Ok(Frame {
        data,
        size: Size::new(f64::from(width), f64::from(height)),
        stride,
        // WGC hands back `B8G8R8A8UIntNormalized`. It stays that way: the
        // format travels with the buffer so a recording does not pay a
        // whole-image channel swap on every frame.
        format: PixelFormat::BgraPremultiplied8,
        color_space: ColorSpace::Srgb,
        scale,
    })
}

fn create_pool(device: &WgcDevice, size: SizeInt32) -> Result<Direct3D11CaptureFramePool> {
    // `CreateFreeThreaded`, not `Create`: the latter binds the pool to the
    // calling thread's `DispatcherQueue` and delivers frames through it, which
    // means a still capture from a worker thread would wait forever.
    //
    // Two buffers rather than one. A single buffer forces the compositor to
    // block until the previous frame is released, and while that is invisible
    // for one screenshot it makes the same code unusable for recording later.
    Direct3D11CaptureFramePool::CreateFreeThreaded(
        &device.winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .map_err(|e| Error::Platform(format!("CreateFreeThreaded failed: {e}")))
}

type FrameBytes = (Vec<u8>, usize, u32, u32);

fn poll_frame(device: &WgcDevice, pool: &Direct3D11CaptureFramePool) -> Result<FrameBytes> {
    let deadline = std::time::Instant::now() + FRAME_TIMEOUT;

    loop {
        match pool.TryGetNextFrame() {
            Ok(frame) => {
                let content = frame
                    .ContentSize()
                    .map_err(|_| Error::TargetGone("frame vanished before it was read".into()))?;
                let surface = frame
                    .Surface()
                    .map_err(|e| Error::Platform(format!("Surface failed: {e}")))?;
                let bytes = read_back(device, &surface, content);
                let _ = frame.Close();
                return bytes;
            }
            Err(e) => {
                // `TryGetNextFrame` returns a null frame — surfaced here as an
                // error — until the compositor has produced one.
                if std::time::Instant::now() >= deadline {
                    return Err(Error::Platform(format!(
                        "no frame arrived within {}ms: {e}",
                        FRAME_TIMEOUT.as_millis()
                    )));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }
}

/// Copies a GPU surface into CPU memory.
///
/// The frame pool's texture is at least as large as the item was when the pool
/// was created, and often larger, so the copy is a `CopySubresourceRegion` of
/// exactly `ContentSize` rather than a whole-resource copy — the GPU does the
/// cropping for free and the staging texture stays as small as possible.
fn read_back(
    device: &WgcDevice,
    surface: &IDirect3DSurface,
    content: SizeInt32,
) -> Result<FrameBytes> {
    let access: IDirect3DDxgiInterfaceAccess = surface
        .cast()
        .map_err(|e| Error::Platform(format!("surface has no DXGI interface: {e}")))?;
    let source: ID3D11Texture2D = unsafe { access.GetInterface() }
        .map_err(|e| Error::Platform(format!("GetInterface(ID3D11Texture2D) failed: {e}")))?;

    let mut source_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { source.GetDesc(&raw mut source_desc) };

    // Clamp to the real texture. `ContentSize` describes what the item wants,
    // and after a window resize it can briefly exceed what the pool allocated;
    // asking the GPU to copy past the end of a resource is a device-removed
    // error, not a graceful failure.
    let width = u32::try_from(content.Width)
        .unwrap_or(0)
        .min(source_desc.Width);
    let height = u32::try_from(content.Height)
        .unwrap_or(0)
        .min(source_desc.Height);
    if width == 0 || height == 0 {
        return Err(Error::TargetGone("captured surface has no area".into()));
    }

    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0.cast_unsigned(),
        MiscFlags: 0,
    };

    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .device
            .CreateTexture2D(&raw const staging_desc, None, Some(&raw mut staging))
    }
    .map_err(|e| Error::Platform(format!("staging texture creation failed: {e}")))?;
    let staging = staging.ok_or_else(|| {
        Error::Platform("CreateTexture2D succeeded but produced no texture".into())
    })?;

    let region = D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: width,
        bottom: height,
        back: 1,
    };
    unsafe {
        device.context.CopySubresourceRegion(
            &staging,
            0,
            0,
            0,
            0,
            &source,
            0,
            Some(&raw const region),
        );
    }

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        device
            .context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
    }
    .map_err(|e| Error::Platform(format!("Map failed: {e}")))?;

    // `RowPitch` is the whole reason this module exists. It is almost never
    // `width * 4` — drivers align rows to 64, 128 or 256 bytes — and copying
    // as though it were produces the classic image that shears further left on
    // every row. The padding is kept rather than removed: `Frame::stride`
    // carries it, so this is one memcpy per row and no repacking at all.
    let stride = mapped.RowPitch as usize;
    let data = if mapped.pData.is_null() {
        unsafe { device.context.Unmap(&staging, 0) };
        return Err(Error::Platform("Map returned a null pointer".into()));
    } else {
        let len = pixels::buffer_len(stride, height);
        let src = unsafe { core::slice::from_raw_parts(mapped.pData.cast::<u8>(), len) };
        pixels::copy_rows_keeping_stride(src, stride, height)
    };

    unsafe { device.context.Unmap(&staging, 0) };

    Ok((data, stride, width, height))
}
