//! Per-monitor DPI.
//!
//! Windows desktops routinely mix scale factors — a 150% laptop panel beside a
//! 100% external monitor is the common case, not an edge case — so there is no
//! such thing as an app-wide scale factor here. Every [`scrozz_core::Display`]
//! carries the scale of *its own* monitor, read from
//! [`GetDpiForMonitor`].
//!
//! Getting there requires the process to be **per-monitor DPI aware v2**.
//! Under the default (unaware) mode Windows lies to the process: it reports
//! every monitor at 96 DPI, hands back virtualised coordinates, and then
//! bitmap-stretches the result. A screenshot tool that inherits that gets
//! blurry captures at the wrong size on any scaled display, and the failure is
//! silent. Awareness is therefore set as early as any of this code runs.

use std::sync::Once;

use scrozz_core::ScaleFactor;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};

use super::geom::{USER_DEFAULT_SCREEN_DPI, scale_from_dpi};

static DPI_AWARE: Once = Once::new();

/// Makes the process per-monitor DPI aware v2, once.
///
/// Failure is deliberately ignored. `SetProcessDpiAwarenessContext` fails with
/// `ERROR_ACCESS_DENIED` when awareness has already been set — by an
/// application manifest, or by an embedding host — and in that case the right
/// thing to do is respect the existing setting rather than fight it. It also
/// fails on Windows 8.1 and earlier, where the v2 context does not exist and
/// per-monitor awareness is not available at all; the capture still works, it
/// is just subject to the OS's virtualisation.
pub fn ensure_process_dpi_aware() {
    DPI_AWARE.call_once(|| {
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    });
}

/// The effective DPI of a monitor, as a pair of `(x, y)` dots per inch.
///
/// Falls back to 96 — the "100%" baseline — when the call fails, which happens
/// on a monitor that was unplugged between enumeration and this call. A wrong
/// scale is better than no display at all, and the capture itself is done in
/// device pixels regardless, so the only casualty is the reported scale.
#[must_use]
pub fn dpi_for_monitor(monitor: HMONITOR) -> (u32, u32) {
    ensure_process_dpi_aware();
    let mut x = 0u32;
    let mut y = 0u32;
    let ok = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) };
    if ok.is_err() || x == 0 || y == 0 {
        return (USER_DEFAULT_SCREEN_DPI, USER_DEFAULT_SCREEN_DPI);
    }
    (x, y)
}

/// The scale factor of a monitor.
///
/// Windows reports separate horizontal and vertical DPI but has never shipped a
/// configuration where they differ for `MDT_EFFECTIVE_DPI`; the horizontal
/// value is authoritative and the vertical is ignored rather than averaged,
/// which would produce a scale matching neither axis.
#[must_use]
pub fn scale_for_monitor(monitor: HMONITOR) -> ScaleFactor {
    let (dpi_x, _) = dpi_for_monitor(monitor);
    scale_from_dpi(dpi_x)
}
