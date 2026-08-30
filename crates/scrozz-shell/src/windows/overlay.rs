//! Win32 non-activating pinned-window adapter.

use std::ffi::c_void;

use scrozz_core::{Error, LogicalRect, Result, ScaleFactor};
use windows::Win32::{
    Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        HiDpi::GetDpiForWindow,
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            EnumWindows, GWL_EXSTYLE, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
            GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST, IsWindow, LWA_ALPHA,
            MA_NOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
            SetLayeredWindowAttributes, SetWindowDisplayAffinity, SetWindowLongPtrW, SetWindowPos,
            WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WM_MOUSEACTIVATE, WM_NCDESTROY, WS_EX_LAYERED,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
        },
    },
};
use windows::core::BOOL;

use crate::{OverlayBehavior, OverlayLevel, OverlayReport, OverlayWindow};

const SUBCLASS_ID: usize = 0x5343_525A_5A50_494E;
const REQUIRED_EX_STYLE: u32 = WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0 | WS_EX_LAYERED.0;

/// A process-owned top-level window retrofitted with Win32 pin behavior.
#[derive(Debug)]
pub struct WindowsOverlay {
    hwnd: HWND,
    title: String,
    original_ex_style: isize,
    subclassed: bool,
    capture_excluded: bool,
    /// Identity comes from the handle, so a later title change is not a loss.
    adopted_by_handle: bool,
}

unsafe extern "system" fn no_activate_subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    if message == WM_NCDESTROY {
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(no_activate_subclass), SUBCLASS_ID) };
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

struct FindState<'a> {
    title: &'a str,
    process: u32,
    matches: Vec<HWND>,
}

unsafe extern "system" fn find_window(hwnd: HWND, data: LPARAM) -> BOOL {
    let state = data.0 as *mut FindState<'_>;
    if state.is_null() {
        return BOOL(0);
    }
    let state = unsafe { &mut *state };
    if owns_window(hwnd, state.process) && window_title(hwnd) == state.title {
        state.matches.push(hwnd);
    }
    BOOL(1)
}

impl WindowsOverlay {
    /// Finds exactly one process-owned top-level window with `title`.
    pub fn find_by_title(title: &str) -> Result<Option<Self>> {
        let mut state = FindState {
            title,
            process: std::process::id(),
            matches: Vec::new(),
        };
        unsafe {
            EnumWindows(
                Some(find_window),
                LPARAM((&raw mut state).cast::<()>() as isize),
            )
        }
        .map_err(|error| Error::Platform(format!("EnumWindows failed: {error}")))?;
        match state.matches.as_slice() {
            [] => Ok(None),
            [hwnd] => Ok(Some(Self {
                hwnd: *hwnd,
                title: title.to_owned(),
                original_ex_style: unsafe { GetWindowLongPtrW(*hwnd, GWL_EXSTYLE) },
                subclassed: false,
                capture_excluded: false,
                adopted_by_handle: false,
            })),
            matches => Err(Error::Platform(format!(
                "refusing ambiguous Win32 pin title {title:?}: {} process-owned windows matched",
                matches.len()
            ))),
        }
    }

    /// Adopts a live top-level window this process already owns.
    ///
    /// This is the seam the overlay creation hook uses: eframe reports an
    /// `HWND` before the window has a stable title, so identity is taken from
    /// the handle rather than looked up by name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for a null handle and
    /// [`Error::TargetGone`] when the handle is not a live window owned by this
    /// process.
    ///
    /// # Safety
    ///
    /// `hwnd` must name a live top-level window owned by this process, and the
    /// returned adapter must not outlive it.
    pub unsafe fn from_hwnd(hwnd: *mut c_void) -> Result<Self> {
        if hwnd.is_null() {
            return Err(Error::InvalidRequest(
                "the overlay window handle is null".to_owned(),
            ));
        }
        let hwnd = HWND(hwnd);
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() || !owns_window(hwnd, std::process::id()) {
            return Err(Error::TargetGone(
                "the overlay window handle does not name a live window owned by this process"
                    .to_owned(),
            ));
        }
        Ok(Self {
            hwnd,
            title: window_title(hwnd),
            original_ex_style: unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) },
            subclassed: false,
            capture_excluded: false,
            adopted_by_handle: true,
        })
    }

    /// Applies non-activation, tool-window, alpha, stacking, and click-through.
    pub fn apply(&mut self, behavior: &OverlayBehavior) -> Result<OverlayReport> {
        self.validate()?;
        let mut style = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) };
        style |= REQUIRED_EX_STYLE as isize;
        if behavior.click_through {
            style |= WS_EX_TRANSPARENT.0 as isize;
        } else {
            style &= !(WS_EX_TRANSPARENT.0 as isize);
        }
        unsafe { SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, style) };

        if !self.subclassed {
            let installed =
                unsafe { SetWindowSubclass(self.hwnd, Some(no_activate_subclass), SUBCLASS_ID, 0) };
            if !installed.as_bool() {
                return Err(Error::Platform(
                    "SetWindowSubclass refused the WM_MOUSEACTIVATE guard".into(),
                ));
            }
            self.subclassed = true;
        }

        let insert_after = if behavior.level == OverlayLevel::Normal {
            HWND_NOTOPMOST
        } else {
            HWND_TOPMOST
        };
        unsafe {
            SetWindowPos(
                self.hwnd,
                Some(insert_after),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            )
        }
        .map_err(|error| Error::Platform(format!("SetWindowPos stacking failed: {error}")))?;

        let alpha = (behavior.opacity.get() * 255.0).round().clamp(0.0, 255.0) as u8;
        unsafe { SetLayeredWindowAttributes(self.hwnd, COLORREF(0), alpha, LWA_ALPHA) }
            .map_err(|error| Error::Platform(format!("window opacity failed: {error}")))?;

        // Display affinity is what actually keeps Scrozz's own chrome out of a
        // screen recording. It is applied here rather than at creation so a
        // surface that must be *visible* to a capture (an ordinary Settings or
        // editor window) can ask for the opposite and get it.
        self.set_capture_excluded(behavior.capture_excluded)?;

        Ok(OverlayReport {
            non_activating: true,
            detail: if self.capture_excluded {
                "Win32 WS_EX_NOACTIVATE + WM_MOUSEACTIVATE guard + WDA_EXCLUDEFROMCAPTURE".into()
            } else {
                "Win32 WS_EX_NOACTIVATE + WM_MOUSEACTIVATE guard".into()
            },
        })
    }

    /// Whether this window is currently hidden from display capture.
    #[must_use]
    pub const fn is_capture_excluded(&self) -> bool {
        self.capture_excluded
    }

    fn set_capture_excluded(&mut self, excluded: bool) -> Result<()> {
        let affinity = if excluded {
            WDA_EXCLUDEFROMCAPTURE
        } else {
            WDA_NONE
        };
        unsafe { SetWindowDisplayAffinity(self.hwnd, affinity) }.map_err(|error| {
            Error::Platform(format!(
                "Windows could not {} the overlay from display capture: {error}",
                if excluded { "exclude" } else { "restore" }
            ))
        })?;
        self.capture_excluded = excluded;
        Ok(())
    }

    /// Removes Scrozz-owned subclass/style changes before winit destroys the window.
    pub fn restore_native_class(&mut self) -> Result<()> {
        if !unsafe { IsWindow(Some(self.hwnd)) }.as_bool() {
            self.subclassed = false;
            self.capture_excluded = false;
            return Ok(());
        }
        if self.capture_excluded {
            self.set_capture_excluded(false)?;
        }
        if self.subclassed {
            let removed =
                unsafe { RemoveWindowSubclass(self.hwnd, Some(no_activate_subclass), SUBCLASS_ID) };
            if !removed.as_bool() {
                return Err(Error::Platform(
                    "RemoveWindowSubclass refused the pin activation guard".into(),
                ));
            }
            self.subclassed = false;
        }
        unsafe { SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, self.original_ex_style) };
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if !unsafe { IsWindow(Some(self.hwnd)) }.as_bool()
            || !owns_window(self.hwnd, std::process::id())
            || (!self.adopted_by_handle && window_title(self.hwnd) != self.title)
        {
            return Err(Error::TargetGone(format!(
                "Win32 pinned window {:?} no longer uniquely identifies this process window",
                self.title
            )));
        }
        Ok(())
    }
}

impl OverlayWindow for WindowsOverlay {
    fn set_frame(&mut self, frame: LogicalRect) -> Result<()> {
        let dpi = unsafe { GetDpiForWindow(self.hwnd) }.max(96);
        self.set_frame_with_scale(frame, ScaleFactor::new(f64::from(dpi) / 96.0))
    }

    fn set_frame_with_scale(&mut self, frame: LogicalRect, scale: ScaleFactor) -> Result<()> {
        self.validate()?;
        let scale = scale.get();
        let x = checked_i32(frame.origin.x * scale, "x")?;
        let y = checked_i32(frame.origin.y * scale, "y")?;
        let width = checked_i32(frame.size.width * scale, "width")?;
        let height = checked_i32(frame.size.height * scale, "height")?;
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                x,
                y,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
        .map_err(|error| Error::Platform(format!("SetWindowPos geometry failed: {error}")))
    }

    fn set_click_through(&mut self, passthrough: bool) -> Result<()> {
        self.validate()?;
        let mut style = unsafe { GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) };
        if passthrough {
            style |= WS_EX_TRANSPARENT.0 as isize;
        } else {
            style &= !(WS_EX_TRANSPARENT.0 as isize);
        }
        unsafe { SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, style) };
        Ok(())
    }
}

impl Drop for WindowsOverlay {
    fn drop(&mut self) {
        let _ = self.restore_native_class();
    }
}

fn owns_window(hwnd: HWND, expected: u32) -> bool {
    let mut process = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut process)) };
    process == expected
}

fn window_title(hwnd: HWND) -> String {
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; length as usize + 1];
    let written = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if written <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..written as usize])
}

fn checked_i32(value: f64, label: &str) -> Result<i32> {
    if !value.is_finite() || value < f64::from(i32::MIN) || value > f64::from(i32::MAX) {
        return Err(Error::InvalidRequest(format!(
            "Win32 pin {label} is outside the physical desktop range"
        )));
    }
    Ok(value.round() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_style_contract_contains_every_non_activation_flag() {
        assert_ne!(REQUIRED_EX_STYLE & WS_EX_NOACTIVATE.0, 0);
        assert_ne!(REQUIRED_EX_STYLE & WS_EX_TOOLWINDOW.0, 0);
        assert_ne!(REQUIRED_EX_STYLE & WS_EX_LAYERED.0, 0);
        assert_eq!(REQUIRED_EX_STYLE & WS_EX_TRANSPARENT.0, 0);
    }

    #[test]
    fn physical_window_coordinates_are_checked_before_win32_conversion() {
        assert!(checked_i32(f64::NAN, "x").is_err());
        assert!(checked_i32(f64::from(i32::MAX) + 1.0, "x").is_err());
        assert_eq!(checked_i32(-120.0, "x").unwrap(), -120);
    }
}
