//! Bindings the workspace's `windows` feature set does not currently provide.
//!
//! **Everything in this file is a stand-in for a Cargo feature.** The
//! `scrozz-capture` manifest declares `windows` 0.62 with `Graphics_Capture`,
//! `Win32_Graphics_Direct3D11`, `Win32_Graphics_Dxgi`, `Win32_Graphics_Gdi`,
//! `Win32_UI_WindowsAndMessaging` and `Win32_Foundation`. A correct WGC
//! backend, correct per-monitor DPI and correct window filtering need six more,
//! listed against each item below. This crate is shared with several other
//! agents and its manifest is off-limits, so the missing declarations are
//! reproduced here instead — **transcribed literally from the vendored
//! bindings**, never from memory, so the signatures and vtable layouts are
//! byte-identical to what the feature would have generated.
//!
//! Every item carries a `MISSING FEATURE:` note. When the manifest gains those
//! features, delete this file and `use` the real paths; nothing else needs to
//! change, because the rest of the backend already calls these under the names
//! the `windows` crate uses.
//!
//! The `link!` macro emits `raw-dylib` imports, so none of this needs a Windows
//! SDK to build — which is what lets it type-check from macOS.

#![allow(non_snake_case, non_camel_case_types)]

use core::ffi::c_void;

use windows::{
    Win32::{
        Foundation::{HANDLE, HWND, RECT},
        Graphics::Gdi::{HDC, HMONITOR},
    },
    core::{BOOL, GUID, HRESULT, IInspectable_Vtbl, IUnknown, Interface, PWSTR},
};

// ---------------------------------------------------------------------------
// Plain exports
// ---------------------------------------------------------------------------

// MISSING FEATURE: `Win32_UI_HiDpi`
//   Windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, GetDpiForMonitor}
windows::core::link!("user32.dll" "system" fn SetProcessDpiAwarenessContext(value: *mut c_void) -> BOOL);
windows::core::link!("api-ms-win-shcore-scaling-l1-1-1.dll" "system" fn GetDpiForMonitor(hmonitor: HMONITOR, dpitype: i32, dpix: *mut u32, dpiy: *mut u32) -> HRESULT);

// MISSING FEATURE: `Win32_Graphics_Dwm`
//   Windows::Win32::Graphics::Dwm::DwmGetWindowAttribute
windows::core::link!("dwmapi.dll" "system" fn DwmGetWindowAttribute(hwnd: HWND, dwattribute: u32, pvattribute: *mut c_void, cbattribute: u32) -> HRESULT);

// MISSING FEATURE: `Win32_Storage_Xps`
//   Windows::Win32::Storage::Xps::PrintWindow — yes, really; the Win32 metadata
//   files it under XPS printing because it shares the "render a window to a DC"
//   machinery.
windows::core::link!("user32.dll" "system" fn PrintWindow(hwnd: HWND, hdcblt: HDC, nflags: u32) -> BOOL);

// MISSING FEATURE: `Win32_System_Threading`
//   Windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW}
windows::core::link!("kernel32.dll" "system" fn OpenProcess(dwdesiredaccess: u32, binherithandle: BOOL, dwprocessid: u32) -> HANDLE);
windows::core::link!("kernel32.dll" "system" fn QueryFullProcessImageNameW(hprocess: HANDLE, dwflags: u32, lpexename: PWSTR, lpdwsize: *mut u32) -> BOOL);

// MISSING FEATURE: `Win32_Graphics_Direct3D`
//   `D3D11CreateDevice` itself lives in `Win32_Graphics_Direct3D11`, which *is*
//   declared, but its safe wrapper is gated on `Win32_Graphics_Direct3D` for
//   the `D3D_DRIVER_TYPE` and `D3D_FEATURE_LEVEL` enums in its signature. Both
//   are plain `i32` newtypes, so the raw import below is equivalent.
windows::core::link!("d3d11.dll" "system" fn D3D11CreateDevice(padapter: *mut c_void, drivertype: i32, software: *mut c_void, flags: u32, pfeaturelevels: *const i32, featurelevels: u32, sdkversion: u32, ppdevice: *mut *mut c_void, pfeaturelevel: *mut i32, ppimmediatecontext: *mut *mut c_void) -> HRESULT);

// MISSING FEATURE: `Win32_System_WinRT_Direct3D11`
//   Windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice
windows::core::link!("d3d11.dll" "system" fn CreateDirect3D11DeviceFromDXGIDevice(dxgidevice: *mut c_void, graphicsdevice: *mut *mut c_void) -> HRESULT);

// ---------------------------------------------------------------------------
// Constants that travel with the above
// ---------------------------------------------------------------------------

/// `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`, a sentinel pointer value.
pub const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;
/// `MDT_EFFECTIVE_DPI` — the DPI the user chose, including fractional scaling.
pub const MDT_EFFECTIVE_DPI: i32 = 0;
/// `D3D_DRIVER_TYPE_HARDWARE`.
pub const D3D_DRIVER_TYPE_HARDWARE: i32 = 1;
/// `D3D_DRIVER_TYPE_WARP` — the software rasteriser, used when no GPU will
/// create a device (a headless CI runner, a stripped-down VM, an RDP session
/// with no adapter).
pub const D3D_DRIVER_TYPE_WARP: i32 = 5;
/// `D3D11_SDK_VERSION`.
pub const D3D11_SDK_VERSION: u32 = 7;
/// `D3D11_CREATE_DEVICE_BGRA_SUPPORT` — required for WGC interop.
pub const D3D11_CREATE_DEVICE_BGRA_SUPPORT: u32 = 0x20;
/// `PW_RENDERFULLCONTENT` — makes `PrintWindow` ask DWM for the composed
/// surface instead of sending `WM_PRINT`, which is the only way it captures
/// hardware-accelerated child content. Windows 8.1+.
pub const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;
/// `PROCESS_QUERY_LIMITED_INFORMATION` — enough to read an image name, and
/// unlike `PROCESS_QUERY_INFORMATION` it succeeds against elevated processes.
pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
/// `DirectXPixelFormat::B8G8R8A8UIntNormalized`.
///
/// MISSING FEATURE: `Graphics_DirectX` would supply this as
/// `Windows::Graphics::DirectX::DirectXPixelFormat`.
pub const DXGI_FORMAT_B8G8R8A8_UNORM: i32 = 87;

// ---------------------------------------------------------------------------
// COM interfaces
// ---------------------------------------------------------------------------

/// Declares a COM interface newtype in the shape `define_interface!` produces.
///
/// The generated macro cannot be reused directly: its expansion names
/// `::windows_core`, which is not a dependency of this crate — only `windows`
/// is, re-exporting it as `windows::core`.
macro_rules! com_interface {
    ($name:ident, $vtbl:ident, $iid:literal) => {
        #[repr(transparent)]
        #[derive(Clone, PartialEq, Eq)]
        pub struct $name(IUnknown);

        unsafe impl Interface for $name {
            type Vtable = $vtbl;
            const IID: GUID = GUID::from_u128($iid);
        }

        impl core::fmt::Debug for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_tuple(stringify!($name))
                    .field(&Interface::as_raw(self))
                    .finish()
            }
        }
    };
}

/// Takes ownership of a COM out-parameter without an extra `AddRef`.
///
/// `Interface::from_raw` transmutes into a `NonNull`, so a null pointer must be
/// rejected first; a COM method that returns `S_OK` with a null out-parameter
/// is misbehaving, and `E_POINTER` is the honest way to say so.
///
/// # Safety
///
/// `raw` must be a pointer this thread owns a reference to, and `T` must be the
/// interface it was returned as.
pub unsafe fn take_raw<T: Interface>(raw: *mut c_void) -> windows::core::Result<T> {
    if raw.is_null() {
        return Err(windows::core::Error::from_hresult(HRESULT(
            0x8000_4003_u32 as i32,
        )));
    }
    Ok(unsafe { T::from_raw(raw) })
}

// MISSING FEATURE: `Win32_System_WinRT_Graphics_Capture`
//   Windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop
//
// The bridge from a Win32 `HWND`/`HMONITOR` to a WinRT `GraphicsCaptureItem`.
// There is no other way to capture a specific window or monitor without showing
// the system picker UI, so WGC is unreachable without it.
com_interface!(
    IGraphicsCaptureItemInterop,
    IGraphicsCaptureItemInterop_Vtbl,
    0x3628e81b_3cac_4c60_b7f4_23ce0e0c3356
);

/// Vtable transcribed from the vendored bindings.
#[repr(C)]
pub struct IGraphicsCaptureItemInterop_Vtbl {
    /// `IUnknown`.
    pub base__: windows::core::IUnknown_Vtbl,
    /// `CreateForWindow(HWND, REFIID, void**)`.
    pub CreateForWindow:
        unsafe extern "system" fn(*mut c_void, HWND, *const GUID, *mut *mut c_void) -> HRESULT,
    /// `CreateForMonitor(HMONITOR, REFIID, void**)`.
    pub CreateForMonitor:
        unsafe extern "system" fn(*mut c_void, HMONITOR, *const GUID, *mut *mut c_void) -> HRESULT,
}

impl IGraphicsCaptureItemInterop {
    /// Creates a capture item for a window.
    ///
    /// # Errors
    ///
    /// Fails with `E_INVALIDARG` for a destroyed or non-capturable `HWND`.
    ///
    /// # Safety
    ///
    /// `hwnd` must be a valid window handle.
    pub unsafe fn CreateForWindow<T: Interface>(&self, hwnd: HWND) -> windows::core::Result<T> {
        let mut out = core::ptr::null_mut();
        unsafe {
            (Interface::vtable(self).CreateForWindow)(
                Interface::as_raw(self),
                hwnd,
                &T::IID,
                &mut out,
            )
            .ok()?;
            take_raw(out)
        }
    }

    /// Creates a capture item for a monitor.
    ///
    /// # Errors
    ///
    /// Fails if the monitor has been disconnected.
    ///
    /// # Safety
    ///
    /// `hmonitor` must be a valid monitor handle.
    pub unsafe fn CreateForMonitor<T: Interface>(
        &self,
        hmonitor: HMONITOR,
    ) -> windows::core::Result<T> {
        let mut out = core::ptr::null_mut();
        unsafe {
            (Interface::vtable(self).CreateForMonitor)(
                Interface::as_raw(self),
                hmonitor,
                &T::IID,
                &mut out,
            )
            .ok()?;
            take_raw(out)
        }
    }
}

// MISSING FEATURE: `Win32_System_WinRT_Direct3D11`
//   Windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess
//
// Unwraps a WinRT `IDirect3DSurface` back to the `ID3D11Texture2D` underneath,
// which is the only way to get at a captured frame's pixels.
com_interface!(
    IDirect3DDxgiInterfaceAccess,
    IDirect3DDxgiInterfaceAccess_Vtbl,
    0xa9b3d012_3df2_4ee3_b8d1_8695f457d3c1
);

/// Vtable transcribed from the vendored bindings.
#[repr(C)]
pub struct IDirect3DDxgiInterfaceAccess_Vtbl {
    /// `IUnknown`.
    pub base__: windows::core::IUnknown_Vtbl,
    /// `GetInterface(REFIID, void**)`.
    pub GetInterface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
}

impl IDirect3DDxgiInterfaceAccess {
    /// Queries the wrapped DXGI/D3D11 interface.
    ///
    /// # Errors
    ///
    /// Fails with `E_NOINTERFACE` if the surface is not backed by `T`.
    ///
    /// # Safety
    ///
    /// The surface must still be alive; a frame's surface dies with the frame.
    pub unsafe fn GetInterface<T: Interface>(&self) -> windows::core::Result<T> {
        let mut out = core::ptr::null_mut();
        unsafe {
            (Interface::vtable(self).GetInterface)(Interface::as_raw(self), &T::IID, &mut out)
                .ok()?;
            take_raw(out)
        }
    }
}

// MISSING FEATURE: `Graphics_DirectX_Direct3D11`
//   Without it the `windows` crate still generates the `Direct3D11CaptureFrame`
//   and `Direct3D11CaptureFramePool` classes, but replaces the two methods
//   whose signatures mention `IDirect3DDevice`/`IDirect3DSurface` with `usize`
//   padding. The vtable *slots* survive — that is what makes the two shims
//   below sound — but the safe wrappers do not, so the pool cannot be created
//   and a frame's surface cannot be read.
//
// Both vtables are transcribed field-for-field from the vendored source,
// including the fields this crate never calls, so every slot index matches.

com_interface!(
    IDirect3D11CaptureFramePoolStatics2,
    IDirect3D11CaptureFramePoolStatics2_Vtbl,
    0x589b103f_6bbc_5df5_a991_02e28b3b66d5
);

/// Vtable transcribed from the vendored bindings.
#[repr(C)]
pub struct IDirect3D11CaptureFramePoolStatics2_Vtbl {
    /// `IInspectable`, itself beginning with `IUnknown`.
    pub base__: IInspectable_Vtbl,
    /// `CreateFreeThreaded(IDirect3DDevice, DirectXPixelFormat, i32, SizeInt32, **)`.
    pub CreateFreeThreaded: unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        i32,
        i32,
        windows::Graphics::SizeInt32,
        *mut *mut c_void,
    ) -> HRESULT,
}

com_interface!(
    IDirect3D11CaptureFrameSurface,
    IDirect3D11CaptureFrameSurface_Vtbl,
    0xfa50c623_38da_4b32_acf3_fa9734ad800e
);

/// Vtable transcribed from the vendored bindings' `IDirect3D11CaptureFrame`.
///
/// Named for the one slot this crate uses so it cannot be confused with the
/// real projected interface, which the `windows` crate also defines.
#[repr(C)]
pub struct IDirect3D11CaptureFrameSurface_Vtbl {
    /// `IInspectable`.
    pub base__: IInspectable_Vtbl,
    /// `Surface(IDirect3DSurface**)`.
    pub Surface: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    /// `SystemRelativeTime(TimeSpan*)` — present for slot alignment only.
    pub SystemRelativeTime:
        unsafe extern "system" fn(*mut c_void, *mut windows::Foundation::TimeSpan) -> HRESULT,
    /// `ContentSize(SizeInt32*)` — present for slot alignment only.
    pub ContentSize:
        unsafe extern "system" fn(*mut c_void, *mut windows::Graphics::SizeInt32) -> HRESULT,
}

impl IDirect3D11CaptureFrameSurface {
    /// The frame's `IDirect3DSurface`, as an opaque `IUnknown`.
    ///
    /// Returned untyped because naming `IDirect3DSurface` needs the very
    /// feature this shim exists to work around; the only thing done with it is
    /// a cast to [`IDirect3DDxgiInterfaceAccess`], which does not care.
    ///
    /// # Errors
    ///
    /// Fails if the frame has already been closed.
    ///
    /// # Safety
    ///
    /// The frame must not have been closed.
    pub unsafe fn Surface(&self) -> windows::core::Result<IUnknown> {
        let mut out = core::ptr::null_mut();
        unsafe {
            (Interface::vtable(self).Surface)(Interface::as_raw(self), &mut out).ok()?;
            take_raw(out)
        }
    }
}

// ---------------------------------------------------------------------------
// Thin safe wrappers over the plain exports
// ---------------------------------------------------------------------------

/// `MONITORINFOEXW`, which `Win32_Graphics_Gdi` does declare but which is
/// easier to use here as a plain struct with the device name inline.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MonitorInfoExW {
    /// Must be set to `size_of::<Self>()` before the call.
    pub cbSize: u32,
    /// Full monitor bounds in virtual-desktop device pixels.
    pub rcMonitor: RECT,
    /// Bounds excluding the taskbar and any other appbars.
    pub rcWork: RECT,
    /// `MONITORINFOF_PRIMARY` when this is the primary monitor.
    pub dwFlags: u32,
    /// `\\.\DISPLAY1`-style device name.
    pub szDevice: [u16; 32],
}

impl Default for MonitorInfoExW {
    fn default() -> Self {
        Self {
            cbSize: core::mem::size_of::<Self>() as u32,
            rcMonitor: RECT::default(),
            rcWork: RECT::default(),
            dwFlags: 0,
            szDevice: [0; 32],
        }
    }
}

/// `MONITORINFOF_PRIMARY`.
pub const MONITORINFOF_PRIMARY: u32 = 1;

// ---------------------------------------------------------------------------
// D3D11 texture shims
// ---------------------------------------------------------------------------

// MISSING FEATURE: `Win32_Graphics_Dxgi_Common`
//   A surprising one. `Win32_Graphics_Direct3D11` *is* declared, but the
//   three calls needed to read a captured texture back to the CPU —
//   `ID3D11Device::CreateTexture2D`, `ID3D11Texture2D::GetDesc` and the
//   `D3D11_TEXTURE2D_DESC` they share — are all gated on
//   `Win32_Graphics_Dxgi_Common`, because the descriptor mentions
//   `DXGI_FORMAT`. Without it a staging texture cannot be created and a WGC
//   frame cannot be mapped, so the whole readback path is unreachable.
//
// The vendored vtables keep the slots (`#[cfg(not(...))] CreateTexture2D:
// usize`), so redeclaring the two vtables with matching layout is sound; the
// fields this crate does not call are left as `usize` for exactly that reason.

/// `DXGI_SAMPLE_DESC`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DxgiSampleDesc {
    /// Multisample count; 1 for a capture texture.
    pub Count: u32,
    /// Multisample quality; 0 for a capture texture.
    pub Quality: u32,
}

/// `D3D11_TEXTURE2D_DESC`, field-for-field.
///
/// `Format` and `Usage` are the underlying `i32` of `DXGI_FORMAT` and
/// `D3D11_USAGE`; both are `#[repr(transparent)]` newtypes over `i32`, so the
/// layout is identical.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Texture2DDesc {
    /// Width in pixels.
    pub Width: u32,
    /// Height in pixels.
    pub Height: u32,
    /// Mip level count; 1 for a capture texture.
    pub MipLevels: u32,
    /// Array size; 1 for a capture texture.
    pub ArraySize: u32,
    /// `DXGI_FORMAT`.
    pub Format: i32,
    /// Multisampling.
    pub SampleDesc: DxgiSampleDesc,
    /// `D3D11_USAGE`.
    pub Usage: i32,
    /// `D3D11_BIND_FLAG` bits; 0 for staging.
    pub BindFlags: u32,
    /// `D3D11_CPU_ACCESS_FLAG` bits.
    pub CPUAccessFlags: u32,
    /// `D3D11_RESOURCE_MISC_FLAG` bits.
    pub MiscFlags: u32,
}

/// `D3D11_USAGE_STAGING`.
pub const D3D11_USAGE_STAGING: i32 = 3;
/// `D3D11_CPU_ACCESS_READ`.
pub const D3D11_CPU_ACCESS_READ: u32 = 0x0002_0000;

com_interface!(
    ID3D11DeviceTextures,
    ID3D11DeviceTextures_Vtbl,
    0xdb6f6ddb_ac77_4e88_8253_819df9bbf140
);

/// `ID3D11Device`'s vtable, truncated after the one slot this crate needs.
#[repr(C)]
pub struct ID3D11DeviceTextures_Vtbl {
    /// `IUnknown`.
    pub base__: windows::core::IUnknown_Vtbl,
    /// `CreateBuffer` — slot padding.
    pub CreateBuffer: usize,
    /// `CreateTexture1D` — slot padding.
    pub CreateTexture1D: usize,
    /// `CreateTexture2D(desc, initial, out)`.
    pub CreateTexture2D: unsafe extern "system" fn(
        *mut c_void,
        *const Texture2DDesc,
        *const c_void,
        *mut *mut c_void,
    ) -> HRESULT,
}

impl ID3D11DeviceTextures {
    /// Creates a texture.
    ///
    /// # Errors
    ///
    /// Fails with `E_OUTOFMEMORY` for an over-large surface, or
    /// `E_INVALIDARG` for a descriptor the driver rejects.
    ///
    /// # Safety
    ///
    /// `desc` must describe a texture the device supports.
    pub unsafe fn CreateTexture2D<T: Interface>(
        &self,
        desc: &Texture2DDesc,
    ) -> windows::core::Result<T> {
        let mut out = core::ptr::null_mut();
        unsafe {
            (Interface::vtable(self).CreateTexture2D)(
                Interface::as_raw(self),
                desc,
                core::ptr::null(),
                &mut out,
            )
            .ok()?;
            take_raw(out)
        }
    }
}

com_interface!(
    ID3D11Texture2DDesc,
    ID3D11Texture2DDesc_Vtbl,
    0x6f15aaf2_d208_4e89_9ab4_489535d34f9c
);

/// `ID3D11Texture2D`'s vtable.
///
/// The base is the vendored `ID3D11Resource_Vtbl`, which is public and not
/// feature-gated, so only the final `GetDesc` slot has to be respelled.
#[repr(C)]
pub struct ID3D11Texture2DDesc_Vtbl {
    /// `ID3D11Resource`.
    pub base__: windows::Win32::Graphics::Direct3D11::ID3D11Resource_Vtbl,
    /// `GetDesc(D3D11_TEXTURE2D_DESC*)`.
    pub GetDesc: unsafe extern "system" fn(*mut c_void, *mut Texture2DDesc),
}

impl ID3D11Texture2DDesc {
    /// Reads the texture's descriptor.
    ///
    /// # Safety
    ///
    /// The texture must still be alive.
    #[must_use]
    pub unsafe fn GetDesc(&self) -> Texture2DDesc {
        let mut desc = Texture2DDesc::default();
        unsafe { (Interface::vtable(self).GetDesc)(Interface::as_raw(self), &mut desc) };
        desc
    }
}

