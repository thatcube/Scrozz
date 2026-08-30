//! Global pointer location in Scrozz's top-left logical coordinate space.

#[cfg(not(target_os = "macos"))]
use scrozz_core::Error;
use scrozz_core::{LogicalPoint, Result};

/// Reads the current global pointer location without consuming input events.
///
/// # Errors
///
/// Returns [`Error::Unsupported`] when a Wayland session exposes no XWayland
/// pointer, or [`Error::Platform`] when the native desktop API fails.
pub fn pointer_location() -> Result<LogicalPoint> {
    platform::pointer_location()
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub fn pointer_location() -> Result<LogicalPoint> {
        crate::macos::display::pointer_location()
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use std::sync::Once;

    use windows::Win32::{
        Foundation::POINT,
        Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint},
        UI::{
            HiDpi::{
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
                SetProcessDpiAwarenessContext,
            },
            WindowsAndMessaging::GetCursorPos,
        },
    };

    use super::*;

    static DPI_AWARE: Once = Once::new();

    pub fn pointer_location() -> Result<LogicalPoint> {
        DPI_AWARE.call_once(|| {
            // Access denied means a manifest or embedding host already selected
            // the process context, which must be respected rather than replaced.
            let _ = unsafe {
                SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
            };
        });

        let mut point = POINT::default();
        unsafe { GetCursorPos(&raw mut point) }
            .map_err(|error| Error::Platform(format!("GetCursorPos failed: {error}")))?;
        let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        let mut dpi_x = 96;
        let mut dpi_y = 96;
        if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_err()
            || dpi_x == 0
        {
            dpi_x = 96;
        }
        let scale = f64::from(dpi_x) / 96.0;
        Ok(LogicalPoint::new(
            f64::from(point.x) / scale,
            f64::from(point.y) / scale,
        ))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use x11rb::{
        connection::Connection,
        protocol::xproto::{AtomEnum, ConnectionExt as _},
        rust_connection::RustConnection,
    };

    use super::*;

    pub fn pointer_location() -> Result<LogicalPoint> {
        if std::env::var_os("DISPLAY").is_none() {
            return Err(Error::Unsupported {
                what: "reading the global pointer on Wayland".to_owned(),
                why: "the Wayland security model does not expose global pointer coordinates. \
                      Run under X11/XWayland, or use region selection instead"
                    .to_owned(),
            });
        }

        let (connection, screen) = RustConnection::connect(None)
            .map_err(|error| Error::Platform(format!("connecting to X11 failed: {error}")))?;
        let root = connection
            .setup()
            .roots
            .get(screen)
            .ok_or_else(|| Error::Platform("X11 reported no default screen".to_owned()))?
            .root;
        let pointer = connection
            .query_pointer(root)
            .map_err(x11_error)?
            .reply()
            .map_err(x11_error)?;
        let scale = scale(&connection, root);
        Ok(LogicalPoint::new(
            f64::from(pointer.root_x) / scale,
            f64::from(pointer.root_y) / scale,
        ))
    }

    fn scale(connection: &RustConnection, root: u32) -> f64 {
        let resources = connection
            .get_property(
                false,
                root,
                AtomEnum::RESOURCE_MANAGER,
                AtomEnum::STRING,
                0,
                65_536,
            )
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| String::from_utf8_lossy(&reply.value).into_owned());

        std::env::var("GDK_SCALE")
            .ok()
            .and_then(|value| plausible_scale(&value))
            .or_else(|| {
                std::env::var("QT_SCALE_FACTOR")
                    .ok()
                    .and_then(|value| plausible_scale(&value))
            })
            .or_else(|| resources.as_deref().and_then(xft_scale))
            .unwrap_or(1.0)
    }

    fn plausible_scale(value: &str) -> Option<f64> {
        let scale = value.trim().parse::<f64>().ok()?;
        (scale.is_finite() && (0.5..=6.0).contains(&scale)).then_some(scale)
    }

    fn xft_scale(resources: &str) -> Option<f64> {
        resources.lines().find_map(|line| {
            let (name, value) = line.trim().split_once(':')?;
            if !name.trim().eq_ignore_ascii_case("Xft.dpi") {
                return None;
            }
            let scale = value.trim().parse::<f64>().ok()? / 96.0;
            (scale.is_finite() && (0.5..=6.0).contains(&scale)).then_some(scale)
        })
    }

    fn x11_error(error: impl std::fmt::Display) -> Error {
        Error::Platform(format!("reading the X11 pointer failed: {error}"))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod platform {
    use super::*;

    pub fn pointer_location() -> Result<LogicalPoint> {
        Err(Error::Unsupported {
            what: "reading the global pointer".to_owned(),
            why: "this platform has no pointer backend".to_owned(),
        })
    }
}
