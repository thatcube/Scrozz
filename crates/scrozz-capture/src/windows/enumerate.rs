//! Monitor and window enumeration.
//!
//! Shared by both capture paths so the target list a user sees never depends on
//! which path ends up doing the work.

use scrozz_core::{Display, DisplayId, Error, Result, ScaleFactor, Window, WindowId};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::MONITORINFOEXW;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HWND, LPARAM, POINT, RECT},
        Graphics::Gdi::{
            EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST,
            MONITORINFO, MonitorFromPoint, MonitorFromWindow,
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GA_ROOTOWNER, GWL_EXSTYLE, GetAncestor, GetClassNameW, GetCursorPos,
            GetShellWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
            GetWindowThreadProcessId, IsIconic, IsWindowVisible,
        },
    },
    core::BOOL,
};

use super::{
    dpi,
    filter::{self, WindowFacts},
    geom::{DeviceRect, dominant_monitor, logical_from_device},
};

/// One enumerated monitor, keeping the raw handle the capture path needs.
#[derive(Debug, Clone)]
pub struct MonitorRecord {
    /// Live handle. Only valid until the display configuration changes.
    pub handle: HMONITOR,
    /// `\\.\DISPLAY1`-style device name, used as the stable id.
    ///
    /// Preferred over the `HMONITOR` value, which is recycled when monitors are
    /// hot-plugged and would silently retarget a capture at a different screen.
    pub device_name: String,
    /// Full bounds in virtual-desktop device pixels. May be negative.
    pub bounds: DeviceRect,
    /// Bounds minus the taskbar and any other appbars.
    pub work_area: DeviceRect,
    /// This monitor's own scale.
    pub scale: ScaleFactor,
    /// Whether this is the primary monitor, i.e. the one containing the origin.
    pub is_primary: bool,
}

impl MonitorRecord {
    /// The public form.
    #[must_use]
    pub fn to_display(&self) -> Display {
        Display {
            id: DisplayId(self.device_name.clone()),
            name: filter::display_label(&self.device_name, self.is_primary),
            bounds: logical_from_device(self.bounds, self.scale),
            work_area: logical_from_device(self.work_area, self.scale),
            scale: self.scale,
            is_primary: self.is_primary,
        }
    }
}

unsafe extern "system" fn monitor_proc(
    monitor: HMONITOR,
    _hdc: HDC,
    _clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let out = lparam.0 as *mut Vec<HMONITOR>;
    if !out.is_null() {
        unsafe { (*out).push(monitor) };
    }
    BOOL(1)
}

/// Every connected monitor, primary first.
///
/// # Errors
///
/// Returns [`Error::Platform`] if the enumeration itself fails, which in
/// practice means the process has no window station — a service, or a session
/// that has been disconnected.
pub fn monitors() -> Result<Vec<MonitorRecord>> {
    dpi::ensure_process_dpi_aware();

    let mut handles: Vec<HMONITOR> = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(monitor_proc),
            LPARAM(&raw mut handles as isize),
        )
    };
    if !ok.as_bool() {
        return Err(Error::Platform("EnumDisplayMonitors failed".into()));
    }

    let mut records: Vec<MonitorRecord> = handles.into_iter().filter_map(monitor_record).collect();

    if records.is_empty() {
        return Err(Error::Platform("no monitors reported".into()));
    }

    // Primary first, then left to right. Anything that reads "the first
    // display" — a default target, a fallback for a stale id — should land on
    // the one the user thinks of as their main screen.
    records.sort_by_key(|m| (!m.is_primary, m.bounds.left, m.bounds.top));
    Ok(records)
}

/// `MONITORINFOF_PRIMARY`, which win32metadata documents only in prose and so
/// the generated bindings do not emit as a constant.
const MONITORINFOF_PRIMARY: u32 = 1;

fn monitor_record(handle: HMONITOR) -> Option<MonitorRecord> {
    let mut info = MONITORINFOEXW::default();
    // `GetMonitorInfoW` dispatches on `cbSize` and returns FALSE if it is not
    // exactly the size of one of the two known structs. The generated
    // `Default` is `mem::zeroed`, so this assignment is what distinguishes a
    // MONITORINFOEXW — with `szDevice` — from a plain MONITORINFO, and
    // omitting it makes every monitor fail to enumerate.
    info.monitorInfo.cbSize = u32::try_from(size_of::<MONITORINFOEXW>()).ok()?;
    let ok = unsafe { GetMonitorInfoW(handle, (&raw mut info).cast::<MONITORINFO>()) };
    if !ok.as_bool() {
        return None;
    }

    let device_name = {
        let end = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        String::from_utf16_lossy(&info.szDevice[..end])
    };

    Some(MonitorRecord {
        handle,
        device_name,
        bounds: rect_to_device(info.monitorInfo.rcMonitor),
        work_area: rect_to_device(info.monitorInfo.rcWork),
        scale: dpi::scale_for_monitor(handle),
        is_primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

const fn rect_to_device(r: RECT) -> DeviceRect {
    DeviceRect::new(r.left, r.top, r.right, r.bottom)
}

/// Looks up a monitor by the id [`MonitorRecord::to_display`] produced.
///
/// # Errors
///
/// Returns [`Error::TargetGone`] when the monitor is no longer connected,
/// which is the honest answer for a display id captured before the user
/// unplugged a screen.
pub fn monitor_by_id(id: &DisplayId) -> Result<MonitorRecord> {
    monitors()?
        .into_iter()
        .find(|m| m.device_name == id.0)
        .ok_or_else(|| Error::TargetGone(format!("display {} is no longer connected", id.0)))
}

/// The monitor under the pointer.
///
/// # Errors
///
/// Returns [`Error::Platform`] if the pointer position is unavailable, which
/// happens in a session with no input desktop.
pub fn monitor_under_cursor() -> Result<MonitorRecord> {
    dpi::ensure_process_dpi_aware();

    let mut point = POINT::default();
    unsafe { GetCursorPos(&raw mut point) }
        .map_err(|e| Error::Platform(format!("GetCursorPos failed: {e}")))?;
    let handle = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };

    // Re-enumerating rather than building the record directly keeps the scale,
    // work area and id derivation in exactly one place, and costs one extra
    // pass over a list that is almost never longer than four.
    let all = monitors()?;
    Ok(all
        .iter()
        .find(|m| m.handle == handle)
        .or_else(|| all.first())
        .cloned()
        .expect("monitors() rejects an empty list"))
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// One enumerated window.
#[derive(Debug, Clone)]
pub struct WindowRecord {
    /// Live handle.
    pub handle: HWND,
    /// Bounds in virtual-desktop device pixels, from the DWM frame where
    /// available.
    pub bounds: DeviceRect,
    /// Window title, absent when the window has none.
    pub title: Option<String>,
    /// Owning executable's file stem, absent when it could not be read.
    pub application: Option<String>,
    /// Index into the [`monitors`] list.
    pub monitor: usize,
}

struct EnumState {
    monitors: Vec<MonitorRecord>,
    shell: HWND,
    out: Vec<WindowRecord>,
}

unsafe extern "system" fn window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = lparam.0 as *mut EnumState;
    if state.is_null() {
        return BOOL(0);
    }
    let state = unsafe { &mut *state };

    if let Some(record) = unsafe { inspect_window(hwnd, state) } {
        state.out.push(record);
    }
    BOOL(1)
}

unsafe fn inspect_window(hwnd: HWND, state: &EnumState) -> Option<WindowRecord> {
    let facts = unsafe { collect_facts(hwnd, state.shell) };
    if !filter::is_capturable(&facts) {
        return None;
    }

    let bounds = unsafe { window_bounds(hwnd) }?;
    let monitor_rects: Vec<DeviceRect> = state.monitors.iter().map(|m| m.bounds).collect();
    let monitor = dominant_monitor(bounds, &monitor_rects).unwrap_or(0);

    Some(WindowRecord {
        handle: hwnd,
        bounds,
        title: (!facts.title.is_empty()).then(|| facts.title.clone()),
        application: unsafe { window_application(facts.owner_process_id) },
        monitor,
    })
}

unsafe fn collect_facts(hwnd: HWND, shell: HWND) -> WindowFacts {
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let root_owner = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
    let rect = unsafe { window_bounds(hwnd) }.unwrap_or(DeviceRect::new(0, 0, 0, 0));
    let mut owner_process_id = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut owner_process_id)) };

    WindowFacts {
        owner_process_id,
        current_process_id: std::process::id(),
        visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
        minimized: unsafe { IsIconic(hwnd) }.as_bool(),
        cloaked: unsafe { is_cloaked(hwnd) },
        ex_style,
        is_root_owner: root_owner == hwnd,
        is_shell_window: hwnd == shell,
        class_name: unsafe { class_name(hwnd) },
        title: unsafe { window_title(hwnd) },
        width: rect.width(),
        height: rect.height(),
    }
}

/// Whether DWM considers this window cloaked.
///
/// Cloaking is how the shell keeps a suspended UWP app's window alive but
/// invisible, and how virtual desktops hide windows belonging to another
/// desktop. Such windows are still `IsWindowVisible`, still have real bounds
/// and still have plausible titles — filtering on visibility alone leaves a
/// window picker full of entries the user cannot see anywhere on screen, which
/// is the single most common way a Windows capture tool feels broken.
unsafe fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            u32::try_from(size_of::<u32>()).unwrap_or(4),
        )
    };
    ok.is_ok() && cloaked != 0
}

/// The window's bounds, preferring the DWM extended frame.
///
/// `GetWindowRect` includes the invisible resize border DWM adds around every
/// top-level window since Vista — roughly seven device pixels a side at 100%.
/// Using it for a window capture puts a transparent margin around the result
/// and makes every window look mysteriously larger than it is.
unsafe fn window_bounds(hwnd: HWND) -> Option<DeviceRect> {
    let mut frame = RECT::default();
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut frame).cast(),
            u32::try_from(size_of::<RECT>()).unwrap_or(16),
        )
    };
    if ok.is_ok() {
        let rect = rect_to_device(frame);
        if !rect.is_empty() {
            return Some(rect);
        }
    }

    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &raw mut rect) }.ok()?;
    Some(rect_to_device(rect))
}

unsafe fn window_title(hwnd: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len as usize) + 1];
    let written = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if written <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..written as usize])
}

unsafe fn class_name(hwnd: HWND) -> String {
    // 256 is the documented maximum for a registered class name.
    let mut buf = [0u16; 256];
    let written = unsafe { GetClassNameW(hwnd, &mut buf) };
    if written <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..written as usize])
}

/// The file stem of the owning executable, e.g. `firefox`.
///
/// Best-effort: reading another process's image name fails for protected
/// processes and for anything running at a higher integrity level, and a
/// missing application name is not a reason to hide a window the user can
/// plainly see.
unsafe fn window_application(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    if handle.is_invalid() {
        return None;
    }

    let mut buf = [0u16; 260];
    let mut len = u32::try_from(buf.len()).unwrap_or(260);
    let ok = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &raw mut len,
        )
    };
    unsafe { close_handle(handle) };

    if ok.is_err() || len == 0 {
        return None;
    }

    let path = String::from_utf16_lossy(&buf[..len as usize]);
    std::path::Path::new(&path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
}

unsafe fn close_handle(handle: HANDLE) {
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(handle) };
}

/// Every capturable window, front-most first.
///
/// # Errors
///
/// Returns [`Error::Platform`] if `EnumWindows` fails or no monitor can be
/// enumerated.
pub fn windows() -> Result<Vec<WindowRecord>> {
    dpi::ensure_process_dpi_aware();

    let mut state = EnumState {
        monitors: monitors()?,
        shell: unsafe { GetShellWindow() },
        out: Vec::new(),
    };

    // `EnumWindows` walks the top-level Z-order front to back, which is the
    // order the contract asks for, so no sorting happens here. A callback that
    // returns FALSE aborts the walk; this one never does, so the only failure
    // is the call itself.
    unsafe { EnumWindows(Some(window_proc), LPARAM(&raw mut state as isize)) }
        .map_err(|e| Error::Platform(format!("EnumWindows failed: {e}")))?;

    Ok(state.out)
}

/// The public form of a window record.
#[must_use]
pub fn to_window(record: &WindowRecord, monitors: &[MonitorRecord]) -> Window {
    let scale = monitors
        .get(record.monitor)
        .map_or(ScaleFactor::IDENTITY, |m| m.scale);
    let display = monitors.get(record.monitor).map_or_else(
        || DisplayId(String::new()),
        |m| DisplayId(m.device_name.clone()),
    );

    Window {
        id: WindowId((record.handle.0 as isize).to_string()),
        title: record.title.clone(),
        application: record.application.clone(),
        bounds: logical_from_device(record.bounds, scale),
        display,
        is_visible: true,
    }
}

/// Parses a [`WindowId`] back to a handle.
///
/// # Errors
///
/// Returns [`Error::TargetGone`] for an id this backend did not produce.
pub fn handle_from_id(id: &WindowId) -> Result<HWND> {
    id.0.parse::<isize>()
        .map(|raw| HWND(raw as *mut core::ffi::c_void))
        .map_err(|_| Error::TargetGone(format!("not a window id this backend issued: {}", id.0)))
}

/// Finds the record for a live window id.
///
/// # Errors
///
/// Returns [`Error::TargetGone`] when the window has closed or is no longer
/// capturable.
pub fn window_by_id(id: &WindowId) -> Result<(WindowRecord, Vec<MonitorRecord>)> {
    let handle = handle_from_id(id)?;
    let monitors = monitors()?;
    let found = windows()?.into_iter().find(|w| w.handle == handle);
    match found {
        Some(record) => Ok((record, monitors)),
        None => Err(Error::TargetGone(format!("window {} has closed", id.0))),
    }
}

/// The monitor a window mostly sits on, for scale purposes.
#[must_use]
pub fn monitor_for_window(hwnd: HWND, monitors: &[MonitorRecord]) -> Option<usize> {
    let handle = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    monitors.iter().position(|m| m.handle == handle)
}
