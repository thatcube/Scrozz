//! Converts Scrozz targets into WGC capture items and GPU crop rectangles.

use core::ffi::c_void;

use scrozz_core::{CaptureTarget, Error, Result};
use windows::{
    Graphics::{Capture::GraphicsCaptureItem, SizeInt32},
    Win32::{
        Foundation::{E_INVALIDARG, HWND, LPARAM, RECT},
        Graphics::Gdi::{
            EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
        },
        System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop,
        UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, GetDpiForWindow,
            MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext,
        },
    },
};

use super::{
    device::Crop,
    geometry::{MonitorGeometry, RegionCrop, RegionError, resolve_region},
};

/// Everything needed to create a WGC frame pool.
pub struct Source {
    /// WinRT capture item.
    pub item: GraphicsCaptureItem,
    /// Initial WGC frame-pool dimensions.
    pub pool_size: SizeInt32,
    /// Rectangle copied into every encoder input texture.
    pub crop: Crop,
    /// Whether target resizes should replace the initial full-frame crop.
    pub resize_with_content: bool,
    /// Physical pixels per logical point for this source.
    pub backing_scale: f64,
}

#[derive(Clone)]
struct Monitor {
    handle: HMONITOR,
    name: String,
    bounds: RECT,
    scale: f64,
}

struct EnumState {
    handles: Vec<HMONITOR>,
}

unsafe extern "system" fn monitor_callback(
    monitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    state: LPARAM,
) -> windows::core::BOOL {
    let state = state.0 as *mut EnumState;
    if !state.is_null() {
        unsafe { (*state).handles.push(monitor) };
    }
    windows::core::BOOL(1)
}

/// Resolves one public capture target.
pub fn resolve(target: &CaptureTarget) -> Result<Source> {
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    match target {
        CaptureTarget::Display(id) => {
            let monitor = monitors()?
                .into_iter()
                .find(|monitor| monitor.name == id.0)
                .ok_or_else(|| {
                    Error::TargetGone(format!("display {} is no longer connected", id.0))
                })?;
            source_for_monitor(&monitor, None)
        }
        CaptureTarget::Window(id) => {
            let raw = id.0.parse::<isize>().map_err(|_| {
                Error::TargetGone(format!("not a Windows window identifier: {}", id.0))
            })?;
            let hwnd = HWND(raw as *mut c_void);
            let item = item_for_window(hwnd)?;
            let size = item
                .Size()
                .map_err(|_| Error::TargetGone("window closed before recording began".into()))?;
            let (width, height) = checked_size(size)?;
            let dpi = unsafe { GetDpiForWindow(hwnd) };
            Ok(Source {
                item,
                pool_size: size,
                crop: Crop {
                    left: 0,
                    top: 0,
                    width,
                    height,
                },
                resize_with_content: true,
                backing_scale: if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 },
            })
        }
        CaptureTarget::Region(region) => {
            let monitors = monitors()?;
            let geometry: Vec<MonitorGeometry<'_>> = monitors
                .iter()
                .map(|monitor| MonitorGeometry {
                    id: &monitor.name,
                    left: monitor.bounds.left,
                    top: monitor.bounds.top,
                    right: monitor.bounds.right,
                    bottom: monitor.bounds.bottom,
                    scale: monitor.scale,
                })
                .collect();
            let resolved = resolve_region(*region, &geometry).map_err(region_error)?;
            source_for_monitor(&monitors[resolved.monitor_index], Some(resolved))
        }
        CaptureTarget::AllDisplays => Err(Error::Unsupported {
            what: "all-displays recording on Windows".into(),
            why: "Windows.Graphics.Capture exposes one monitor per capture item; select a display \
                  or area"
                .into(),
        }),
    }
}

fn source_for_monitor(monitor: &Monitor, region: Option<RegionCrop>) -> Result<Source> {
    let item = item_for_monitor(monitor.handle)?;
    let size = item
        .Size()
        .map_err(|_| Error::TargetGone("display disconnected before recording began".into()))?;
    let (pool_width, pool_height) = checked_size(size)?;
    let current = monitor_record(monitor.handle).ok_or_else(|| {
        Error::TargetGone(format!(
            "display {} disconnected while recording started",
            monitor.name
        ))
    })?;
    if !same_monitor(monitor, &current) {
        return Err(Error::TargetGone(format!(
            "display {} changed identity or geometry while recording started",
            monitor.name
        )));
    }
    let expected_width = current
        .bounds
        .right
        .checked_sub(current.bounds.left)
        .and_then(|width| u32::try_from(width).ok())
        .unwrap_or(0);
    let expected_height = current
        .bounds
        .bottom
        .checked_sub(current.bounds.top)
        .and_then(|height| u32::try_from(height).ok())
        .unwrap_or(0);
    if pool_width != expected_width || pool_height != expected_height {
        return Err(Error::TargetGone(format!(
            "display {} changed geometry while its capture item was created",
            monitor.name
        )));
    }
    let crop = match region {
        None => Crop {
            left: 0,
            top: 0,
            width: pool_width,
            height: pool_height,
        },
        Some(region) => {
            let right = region.left.checked_add(region.width);
            let bottom = region.top.checked_add(region.height);
            let (Some(right), Some(bottom)) = (right, bottom) else {
                return Err(Error::TargetGone(format!(
                    "display {} returned an overflowing recording crop",
                    monitor.name
                )));
            };
            if region.left >= pool_width
                || region.top >= pool_height
                || right > pool_width
                || bottom > pool_height
            {
                return Err(Error::TargetGone(format!(
                    "display {} changed before its recording crop could be applied",
                    monitor.name
                )));
            }
            Crop {
                left: region.left,
                top: region.top,
                width: region.width,
                height: region.height,
            }
        }
    };
    if crop.width == 0 || crop.height == 0 {
        return Err(Error::InvalidRequest(
            "the recording area has no pixels".into(),
        ));
    }
    Ok(Source {
        item,
        pool_size: size,
        crop,
        resize_with_content: region.is_none(),
        backing_scale: monitor.scale,
    })
}

fn interop() -> Result<IGraphicsCaptureItemInterop> {
    windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(|error| {
        Error::Unsupported {
            what: "Windows.Graphics.Capture".into(),
            why: format!("capture activation factory is unavailable: {error}"),
        }
    })
}

fn item_for_window(hwnd: HWND) -> Result<GraphicsCaptureItem> {
    unsafe { interop()?.CreateForWindow::<GraphicsCaptureItem>(hwnd) }.map_err(|error| {
        if error.code() == E_INVALIDARG {
            Error::TargetGone("window closed or cannot be captured".into())
        } else {
            Error::Platform(format!("CreateForWindow failed: {error}"))
        }
    })
}

fn item_for_monitor(monitor: HMONITOR) -> Result<GraphicsCaptureItem> {
    unsafe { interop()?.CreateForMonitor::<GraphicsCaptureItem>(monitor) }.map_err(|error| {
        if error.code() == E_INVALIDARG {
            Error::TargetGone("display disconnected".into())
        } else {
            Error::Platform(format!("CreateForMonitor failed: {error}"))
        }
    })
}

fn checked_size(size: SizeInt32) -> Result<(u32, u32)> {
    let width = u32::try_from(size.Width).unwrap_or(0);
    let height = u32::try_from(size.Height).unwrap_or(0);
    if width == 0 || height == 0 {
        Err(Error::TargetGone("capture target has no area".into()))
    } else {
        Ok((width, height))
    }
}

fn monitors() -> Result<Vec<Monitor>> {
    let mut state = EnumState {
        handles: Vec::new(),
    };
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(monitor_callback),
            LPARAM((&raw mut state).cast::<c_void>() as isize),
        )
    };
    if !ok.as_bool() {
        return Err(Error::Platform("EnumDisplayMonitors failed".into()));
    }

    let records: Vec<Monitor> = state
        .handles
        .into_iter()
        .filter_map(monitor_record)
        .collect();
    if records.is_empty() {
        Err(Error::TargetGone("no displays are connected".into()))
    } else {
        Ok(records)
    }
}

fn monitor_record(handle: HMONITOR) -> Option<Monitor> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = u32::try_from(size_of::<MONITORINFOEXW>()).ok()?;
    if !unsafe { GetMonitorInfoW(handle, (&raw mut info).cast::<MONITORINFO>()) }.as_bool() {
        return None;
    }
    let end = info
        .szDevice
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(info.szDevice.len());
    let mut x = 96;
    let mut y = 96;
    if unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut x, &mut y) }.is_err() || x == 0 {
        x = 96;
    }
    Some(Monitor {
        handle,
        name: String::from_utf16_lossy(&info.szDevice[..end]),
        bounds: info.monitorInfo.rcMonitor,
        scale: f64::from(x) / 96.0,
    })
}

fn same_monitor(a: &Monitor, b: &Monitor) -> bool {
    a.name == b.name
        && a.bounds.left == b.bounds.left
        && a.bounds.top == b.bounds.top
        && a.bounds.right == b.bounds.right
        && a.bounds.bottom == b.bounds.bottom
        && a.scale == b.scale
}

fn region_error(error: RegionError) -> Error {
    match error {
        RegionError::InvalidGeometry => {
            Error::InvalidRequest("the Windows recording area has invalid geometry".into())
        }
        RegionError::NoDisplay => {
            Error::TargetGone("the recording area overlaps no connected display".into())
        }
        RegionError::AmbiguousDisplays(displays) => Error::InvalidRequest(format!(
            "the Windows recording area cannot be assigned to one display under mixed DPI \
             ({})",
            displays.join(", ")
        )),
        RegionError::CrossesDisplay(display) => Error::InvalidRequest(format!(
            "the Windows recording area crosses the edge of {display}; recording regions must \
             stay within one display"
        )),
    }
}
