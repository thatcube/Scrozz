//! Windows overlay capture exclusion.

use std::ffi::c_void;

use scrozz_core::{Error, Result};
use windows::Win32::{
    Foundation::{ERROR_SUCCESS, GetLastError, HWND, SetLastError},
    UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, SetWindowDisplayAffinity, SetWindowLongPtrW, SetWindowPos,
        WDA_EXCLUDEFROMCAPTURE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    },
};

/// Makes the Scrozz overlay non-activating, topmost, and capture-excluded.
///
/// # Safety
///
/// `hwnd` must identify a live top-level window owned by this process.
pub unsafe fn configure(hwnd: *mut c_void) -> Result<()> {
    if hwnd.is_null() {
        return Err(Error::InvalidRequest(
            "the overlay window handle is null".to_owned(),
        ));
    }
    let hwnd = HWND(hwnd);
    let current = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    let additions = WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0 | WS_EX_LAYERED.0;
    unsafe {
        SetLastError(ERROR_SUCCESS);
    }
    let previous = unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, current | additions as isize) };
    let style_error = unsafe { GetLastError() };
    if previous == 0 && style_error != ERROR_SUCCESS {
        return Err(Error::Platform(format!(
            "Windows could not make the overlay non-activating (Win32 error {})",
            style_error.0
        )));
    }
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
    }
    .map_err(|error| Error::Platform(format!("Windows could not raise the overlay: {error}")))?;
    unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) }.map_err(|error| {
        Error::Platform(format!(
            "Windows could not exclude the overlay from capture: {error}"
        ))
    })
}
