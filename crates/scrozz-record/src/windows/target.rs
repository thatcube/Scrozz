//! Converts Scrozz targets into WGC capture items and GPU crop rectangles.

use core::ffi::c_void;

use scrozz_core::{CaptureTarget, Error, LogicalRect, Result};
use windows::{
    Graphics::{Capture::GraphicsCaptureItem, SizeInt32},
    Win32::{
        Foundation::{E_INVALIDARG, HWND, LPARAM, RECT},
        Graphics::Gdi::{
            EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
        },
        System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop,
        UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
            SetProcessDpiAwarenessContext,
        },
    },
};

use super::device::Crop;

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
            let item = item_for_window(HWND(raw as *mut c_void))?;
            let size = item
                .Size()
                .map_err(|_| Error::TargetGone("window closed before recording began".into()))?;
            let (width, height) = checked_size(size)?;
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
            })
        }
        CaptureTarget::Region(region) => {
            let monitors = monitors()?;
            let monitor = monitors
                .iter()
                .max_by(|a, b| {
                    overlap_area(*region, a)
                        .partial_cmp(&overlap_area(*region, b))
                        .unwrap_or(core::cmp::Ordering::Equal)
                })
                .filter(|monitor| overlap_area(*region, monitor) > 0.0)
                .ok_or_else(|| Error::TargetGone("recording area overlaps no display".into()))?;
            source_for_monitor(monitor, Some(*region))
        }
        CaptureTarget::AllDisplays => Err(Error::Unsupported {
            what: "all-displays recording on Windows".into(),
            why: "Windows.Graphics.Capture exposes one monitor per capture item; select a display \
                  or area"
                .into(),
        }),
    }
}

fn source_for_monitor(monitor: &Monitor, region: Option<LogicalRect>) -> Result<Source> {
    let item = item_for_monitor(monitor.handle)?;
    let size = item
        .Size()
        .map_err(|_| Error::TargetGone("display disconnected before recording began".into()))?;
    let (pool_width, pool_height) = checked_size(size)?;
    let crop = region.map_or(
        Crop {
            left: 0,
            top: 0,
            width: pool_width,
            height: pool_height,
        },
        |region| crop_for_region(region, monitor, pool_width, pool_height),
    );
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

fn logical_bounds(monitor: &Monitor) -> LogicalRect {
    LogicalRect {
        origin: scrozz_core::Point::new(
            f64::from(monitor.bounds.left) / monitor.scale,
            f64::from(monitor.bounds.top) / monitor.scale,
        ),
        size: scrozz_core::Size::new(
            f64::from(monitor.bounds.right - monitor.bounds.left) / monitor.scale,
            f64::from(monitor.bounds.bottom - monitor.bounds.top) / monitor.scale,
        ),
    }
}

fn overlap_area(region: LogicalRect, monitor: &Monitor) -> f64 {
    intersection(region, logical_bounds(monitor))
        .map_or(0.0, |rect| rect.size.width * rect.size.height)
}

fn crop_for_region(
    region: LogicalRect,
    monitor: &Monitor,
    pool_width: u32,
    pool_height: u32,
) -> Crop {
    let bounds = logical_bounds(monitor);
    let clipped = intersection(region, bounds).unwrap_or(LogicalRect {
        origin: bounds.origin,
        size: scrozz_core::Size::new(0.0, 0.0),
    });
    let left = ((clipped.origin.x - bounds.origin.x) * monitor.scale)
        .round()
        .max(0.0) as u32;
    let top = ((clipped.origin.y - bounds.origin.y) * monitor.scale)
        .round()
        .max(0.0) as u32;
    let right = (((clipped.origin.x + clipped.size.width - bounds.origin.x) * monitor.scale)
        .round()
        .max(0.0) as u32)
        .min(pool_width);
    let bottom = (((clipped.origin.y + clipped.size.height - bounds.origin.y) * monitor.scale)
        .round()
        .max(0.0) as u32)
        .min(pool_height);
    Crop {
        left: left.min(right),
        top: top.min(bottom),
        width: right.saturating_sub(left),
        height: bottom.saturating_sub(top),
    }
}

fn intersection(a: LogicalRect, b: LogicalRect) -> Option<LogicalRect> {
    let left = a.origin.x.max(b.origin.x);
    let top = a.origin.y.max(b.origin.y);
    let right = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let bottom = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    (right > left && bottom > top).then(|| LogicalRect {
        origin: scrozz_core::Point::new(left, top),
        size: scrozz_core::Size::new(right - left, bottom - top),
    })
}
