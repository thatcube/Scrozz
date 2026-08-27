//! Target-addressed scroll synthesis through Win32 window messages.

use scrozz_core::{Error, Result, ScrollCapabilities, ScrollDriver, ScrollGesture};
use windows::Win32::{
    Foundation::{GetLastError, HWND, LPARAM, POINT, RECT, SetLastError, WIN32_ERROR, WPARAM},
    Graphics::Gdi::ScreenToClient,
    UI::WindowsAndMessaging::{
        CWP_SKIPDISABLED, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, ChildWindowFromPointEx, GA_ROOT,
        GetAncestor, GetWindowRect, IsIconic, IsWindowVisible, SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT,
        SendMessageTimeoutW, WM_MOUSEHWHEEL, WM_MOUSEWHEEL,
    },
};

use crate::scroll_units;

use super::{dpi, enumerate};

/// Win32 scroll synthesis bound to a snapshotted process/window identity.
#[derive(Debug, Default)]
pub(crate) struct TargetedWheelScrollDriver;

impl TargetedWheelScrollDriver {
    pub(crate) const fn new() -> Self {
        Self
    }

    fn target_device_point(gesture: &ScrollGesture) -> Result<(i32, i32)> {
        if !scroll_units::finite_point(gesture.at) {
            return Err(Error::InvalidRequest(
                "the scroll target point must contain finite coordinates".into(),
            ));
        }

        let monitors = enumerate::monitors()?;
        let selected = if let Some(display) = &gesture.display {
            monitors
                .into_iter()
                .find(|monitor| monitor.device_name == display.0)
                .ok_or_else(|| {
                    Error::TargetGone(format!("display {} is no longer connected", display.0))
                })?
        } else {
            monitors
                .into_iter()
                .find(|monitor| {
                    scroll_units::logical_to_device_point(
                        gesture.at,
                        monitor.to_display().bounds,
                        monitor.scale,
                    )
                    .is_some()
                })
                .ok_or_else(|| {
                    Error::InvalidRequest(format!(
                        "the scroll target ({}, {}) is outside the Windows virtual desktop",
                        gesture.at.x, gesture.at.y
                    ))
                })?
        };
        scroll_units::logical_to_device_point(
            gesture.at,
            selected.to_display().bounds,
            selected.scale,
        )
        .ok_or_else(|| {
            Error::InvalidRequest(format!(
                "the scroll target ({}, {}) is outside display {}",
                gesture.at.x, gesture.at.y, selected.device_name
            ))
        })
    }
}

impl ScrollDriver for TargetedWheelScrollDriver {
    fn capabilities(&self) -> ScrollCapabilities {
        ScrollCapabilities::automatic(false)
    }

    fn prepare(&mut self) -> Result<()> {
        dpi::ensure_process_dpi_aware();
        Ok(())
    }

    fn scroll(&mut self, gesture: &ScrollGesture) -> Result<()> {
        if gesture.is_noop() {
            return Ok(());
        }
        dpi::ensure_process_dpi_aware();

        let target = Self::target_device_point(gesture)?;
        let selected_window = gesture
            .window
            .as_ref()
            .map(enumerate::verified_handle_from_id)
            .transpose()?
            .map(|selected| unsafe { GetAncestor(selected, GA_ROOT) })
            .ok_or_else(|| Error::Unsupported {
                what: "automatic scrolling of an unspecified Windows target".into(),
                why: "Windows wheel messages require the exact selected window before Scrozz can \
                      deliver them safely"
                    .into(),
            })?;
        let mut selected_bounds = RECT::default();
        let selected_is_usable = !selected_window.is_invalid()
            && unsafe { IsWindowVisible(selected_window) }.as_bool()
            && !unsafe { IsIconic(selected_window) }.as_bool()
            && unsafe { GetWindowRect(selected_window, &raw mut selected_bounds) }.is_ok()
            && target.0 >= selected_bounds.left
            && target.0 < selected_bounds.right
            && target.1 >= selected_bounds.top
            && target.1 < selected_bounds.bottom;
        if !selected_is_usable {
            return Err(Error::TargetGone(
                "the selected Windows window is no longer visible at the snapshotted scroll point"
                    .into(),
            ));
        }

        // Resolve inside the verified target rather than through
        // `WindowFromPoint`: Scrozz's own always-on-top HUD may be globally
        // hit-testable while its Keep/Discard controls are under the pointer.
        // Parent-relative lookup cannot escape into that overlay or any other
        // process, so the wheel remains bound to the snapshotted window.
        let recipient = deepest_child_at(
            selected_window,
            POINT {
                x: target.0,
                y: target.1,
            },
        )?;

        let message = match gesture.axis {
            scrozz_core::ScrollAxis::Vertical => WM_MOUSEWHEEL,
            scrozz_core::ScrollAxis::Horizontal => WM_MOUSEHWHEEL,
        };
        let wheel_delta = scroll_units::windows_delta(gesture.axis, gesture.amount);
        let coordinates = wheel_coordinates(target)?;
        let wheel = wheel_parameter(wheel_delta)?;
        let mut result = 0usize;
        unsafe { SetLastError(WIN32_ERROR(0)) };
        let delivered = unsafe {
            SendMessageTimeoutW(
                recipient,
                message,
                wheel,
                coordinates,
                SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
                250,
                Some(&raw mut result),
            )
        };
        if delivered.0 == 0 {
            let last_error = unsafe { GetLastError() };
            return Err(Error::Platform(format!(
                "Windows did not deliver the wheel message to the selected target (Win32 error \
                 {}; UIPI may be blocking a higher-integrity window, or the target may be hung)",
                last_error.0,
            )));
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "Win32 target wheel"
    }
}

fn deepest_child_at(root: HWND, screen: POINT) -> Result<HWND> {
    let mut parent = root;
    for _ in 0..64 {
        let mut local = screen;
        unsafe { ScreenToClient(parent, &raw mut local) }
            .ok()
            .map_err(|error| {
                Error::Platform(format!(
                    "could not map the Windows scroll point into the selected target: {error}"
                ))
            })?;
        let child = unsafe {
            ChildWindowFromPointEx(
                parent,
                local,
                CWP_SKIPDISABLED | CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT,
            )
        };
        if child.is_invalid() || child == parent {
            return Ok(parent);
        }
        parent = child;
    }
    Err(Error::Platform(
        "the selected Windows target has an unexpectedly deep child-window hierarchy".into(),
    ))
}

fn wheel_parameter(delta: i32) -> Result<WPARAM> {
    let delta = i16::try_from(delta).map_err(|_| {
        Error::Platform("Windows wheel delta does not fit its message field".into())
    })?;
    Ok(WPARAM(usize::from(delta.cast_unsigned()) << 16))
}

fn wheel_coordinates(point: (i32, i32)) -> Result<LPARAM> {
    let x = i16::try_from(point.0).map_err(|_| Error::Unsupported {
        what: "automatic scrolling at this Windows desktop coordinate".into(),
        why: "WM_MOUSEWHEEL carries signed 16-bit screen coordinates, and the selected point is \
              outside that range. Scroll manually so the event cannot be misaddressed"
            .into(),
    })?;
    let y = i16::try_from(point.1).map_err(|_| Error::Unsupported {
        what: "automatic scrolling at this Windows desktop coordinate".into(),
        why: "WM_MOUSEWHEEL carries signed 16-bit screen coordinates, and the selected point is \
              outside that range. Scroll manually so the event cannot be misaddressed"
            .into(),
    })?;
    let packed = u32::from(x.cast_unsigned()) | (u32::from(y.cast_unsigned()) << 16);
    Ok(LPARAM(packed as isize))
}
