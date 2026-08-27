//! Scroll synthesis through Win32 `SendInput`.

use std::mem::size_of;

use scrozz_core::{Error, Result, ScrollCapabilities, ScrollDriver, ScrollGesture};
use windows::Win32::{
    Foundation::{GetLastError, POINT, SetLastError, WIN32_ERROR},
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
            MOUSEEVENTF_HWHEEL, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
            MOUSEINPUT, SendInput,
        },
        WindowsAndMessaging::{
            GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
            SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetCursorPos,
        },
    },
};

use crate::scroll_units;

use super::{dpi, enumerate};

/// Win32 scroll synthesis, including point targeting and UIPI error reporting.
#[derive(Debug, Default)]
pub(crate) struct SendInputScrollDriver;

impl SendInputScrollDriver {
    pub(crate) const fn new() -> Self {
        Self
    }

    fn target_device_point(gesture: &ScrollGesture) -> Result<(i32, i32)> {
        if !scroll_units::finite_point(gesture.at) {
            return Err(Error::InvalidRequest(
                "the scroll target point must contain finite coordinates".into(),
            ));
        }

        enumerate::monitors()?
            .into_iter()
            .find_map(|monitor| {
                scroll_units::logical_to_device_point(
                    gesture.at,
                    monitor.to_display().bounds,
                    monitor.scale,
                )
            })
            .ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "the scroll target ({}, {}) is outside the Windows virtual desktop",
                    gesture.at.x, gesture.at.y
                ))
            })
    }
}

impl ScrollDriver for SendInputScrollDriver {
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
        let mut original = POINT::default();
        unsafe { GetCursorPos(&raw mut original) }
            .map_err(|err| Error::Platform(format!("GetCursorPos failed: {err}")))?;

        let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        let move_to_target = absolute_move(target, left, top, width, height)?;
        let move_to_original = absolute_move((original.x, original.y), left, top, width, height)?;

        let flags = match gesture.axis {
            scrozz_core::ScrollAxis::Vertical => MOUSEEVENTF_WHEEL,
            scrozz_core::ScrollAxis::Horizontal => MOUSEEVENTF_HWHEEL,
        };
        let wheel = mouse_input(
            flags,
            scroll_units::windows_delta(gesture.axis, gesture.amount) as u32,
            0,
            0,
        );
        let inputs = [move_to_target, wheel, move_to_original];
        let input_size = i32::try_from(size_of::<INPUT>())
            .map_err(|_| Error::Platform("INPUT structure size does not fit Win32".into()))?;

        // SendInput does not reliably set the last error when UIPI blocks input,
        // so clear it first and always treat a short insertion as failure.
        unsafe { SetLastError(WIN32_ERROR(0)) };
        let inserted = unsafe { SendInput(&inputs, input_size) };
        if inserted != inputs.len() as u32 {
            let last_error = unsafe { GetLastError() };
            let restore = if inserted == 0 {
                Ok(())
            } else {
                unsafe { SetCursorPos(original.x, original.y) }
            };
            let restore_note = restore.err().map_or(String::new(), |err| {
                format!("; restoring the pointer also failed: {err}")
            });
            return Err(Error::Platform(format!(
                "SendInput inserted {inserted} of {} events (Win32 error {}; input may have been \
                 blocked by UIPI because the target runs at a higher integrity level){restore_note}",
                inputs.len(),
                last_error.0
            )));
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "SendInput"
    }
}

fn absolute_move(point: (i32, i32), left: i32, top: i32, width: i32, height: i32) -> Result<INPUT> {
    let x =
        scroll_units::normalized_absolute_coordinate(point.0, left, width).ok_or_else(|| {
            Error::Platform("Windows reported an invalid virtual desktop width".into())
        })?;
    let y =
        scroll_units::normalized_absolute_coordinate(point.1, top, height).ok_or_else(|| {
            Error::Platform("Windows reported an invalid virtual desktop height".into())
        })?;
    Ok(mouse_input(
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        0,
        x,
        y,
    ))
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, data: u32, dx: i32, dy: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
