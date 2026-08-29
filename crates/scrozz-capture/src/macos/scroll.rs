//! Accessibility-gated scroll synthesis through Quartz events.

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGPreflightPostEventAccess, CGRequestPostEventAccess, CGScrollEventUnit,
};
use scrozz_core::{Error, Result, ScaleFactor, ScrollCapabilities, ScrollDriver, ScrollGesture};

use crate::scroll_units;

const CAPABILITY: &str = "Accessibility control for scrolling other applications";
const REMEDY: &str =
    "System Settings → Privacy & Security → Accessibility: switch Scrozz on, then retry";

/// Scroll synthesis through `CGEventPost`.
#[derive(Debug, Default)]
pub(crate) struct CgEventScrollDriver;

impl CgEventScrollDriver {
    pub(crate) const fn new() -> Self {
        Self
    }

    fn ensure_trusted() -> Result<()> {
        // `CGEventPost` has no return value and silently drops input without the
        // Accessibility grant. Preflighting every delivery also catches a grant
        // revoked after `prepare`.
        if CGPreflightPostEventAccess() {
            Ok(())
        } else {
            Err(Error::PermissionDenied {
                capability: CAPABILITY.into(),
                remedy: REMEDY.into(),
            })
        }
    }

    fn ensure_selected_window_owns_point(gesture: &ScrollGesture) -> Result<libc::pid_t> {
        let selected = gesture.window.as_ref().ok_or_else(|| Error::Unsupported {
            what: "automatic scrolling of an unspecified macOS target".into(),
            why: "Quartz wheel events are location-addressed, so Scrozz requires the exact \
                  selected window before it can post one safely"
                .into(),
        })?;
        let selected_id = selected.0.parse::<u32>().map_err(|_| {
            Error::InvalidRequest(format!(
                "window id {} is not a macOS window identity",
                selected.0
            ))
        })?;
        let content = super::sck::shareable_content()?;
        // CoreGraphics documents this list as front-to-back. ScreenCaptureKit
        // supplies geometry and process identity, but its own array order is not
        // used as an input-delivery claim.
        let ordered = super::window::on_screen_window_ids_front_to_back()?;
        // SAFETY: property reads on one immutable shareable-content snapshot.
        let windows = unsafe { content.windows() };
        let selected_window = windows
            .iter()
            .find(|window| unsafe { window.windowID() == selected_id })
            .ok_or_else(|| {
                Error::TargetGone(format!(
                    "window {} is no longer present in ScreenCaptureKit",
                    selected.0
                ))
            })?;
        let selected_pid = unsafe {
            selected_window
                .owningApplication()
                .map(|application| application.processID())
        }
        .ok_or_else(|| {
            Error::TargetGone(format!(
                "window {} no longer has an owning process",
                selected.0
            ))
        })?;
        let current_pid = i32::try_from(std::process::id()).map_err(|_| {
            Error::Platform("the Scrozz process id does not fit macOS pid_t".into())
        })?;
        let frontmost = ordered.into_iter().find_map(|window_id| {
            windows
                .iter()
                .find(|window| unsafe { window.windowID() == window_id })
                // The transparent Scrozz overlay spans the work area above the
                // target. It is deliberately mouse-transparent and therefore
                // cannot be the input owner even though it is visually first.
                .filter(|window| unsafe {
                    window
                        .owningApplication()
                        .is_none_or(|application| application.processID() != current_pid)
                })
                .filter(|window| unsafe {
                    window.isOnScreen()
                        && contains(super::display::from_cg_rect(window.frame()), gesture.at)
                })
        });
        match frontmost {
            Some(window) if unsafe { window.windowID() } == selected_id => Ok(selected_pid),
            Some(window) => Err(Error::TargetGone(format!(
                "window {} no longer owns the scroll point; it is covered by window {}",
                selected.0,
                unsafe { window.windowID() }
            ))),
            None => Err(Error::TargetGone(format!(
                "window {} is no longer visible at the selected scroll point",
                selected.0
            ))),
        }
    }

    fn post(pid: libc::pid_t, gesture: &ScrollGesture) -> Result<()> {
        let (wheel1, wheel2) = scroll_units::macos_deltas(gesture.axis, gesture.amount);
        let event =
            CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, wheel1, wheel2, 0)
                .ok_or_else(|| {
                    Error::Platform("CGEventCreateScrollWheelEvent2 returned null".into())
                })?;

        CGEvent::set_location(Some(&event), CGPoint::new(gesture.at.x, gesture.at.y));
        CGEvent::post_to_pid(pid, Some(&event));
        Ok(())
    }
}

impl ScrollDriver for CgEventScrollDriver {
    fn capabilities(&self) -> ScrollCapabilities {
        ScrollCapabilities::automatic(true)
    }

    fn expected_physical_delta(
        &self,
        gesture: &ScrollGesture,
        frame_scale: ScaleFactor,
    ) -> Option<u32> {
        let physical = gesture.amount * frame_scale.get();
        (physical.is_finite() && physical > 0.0)
            .then(|| physical.round().clamp(1.0, f64::from(u32::MAX)) as u32)
    }

    fn prepare(&mut self) -> Result<()> {
        if !CGPreflightPostEventAccess() {
            let _ = CGRequestPostEventAccess();
        }
        // Do not infer success from the request call: posting without a current
        // grant is silently ignored by Quartz.
        Self::ensure_trusted()
    }

    fn scroll(&mut self, gesture: &ScrollGesture) -> Result<()> {
        if gesture.is_noop() {
            return Ok(());
        }
        if !scroll_units::finite_point(gesture.at) {
            return Err(Error::InvalidRequest(
                "the scroll target point must contain finite coordinates".into(),
            ));
        }
        Self::ensure_trusted()?;
        let pid = Self::ensure_selected_window_owns_point(gesture)?;
        Self::post(pid, gesture)?;
        let current_pid = Self::ensure_selected_window_owns_point(gesture)?;
        if current_pid != pid {
            return Err(Error::TargetGone(format!(
                "window {} changed owning process while scrolling",
                gesture
                    .window
                    .as_ref()
                    .expect("ownership was checked above")
                    .0
            )));
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "CGEvent"
    }
}

fn contains(rect: scrozz_core::LogicalRect, point: scrozz_core::LogicalPoint) -> bool {
    point.x >= rect.origin.x
        && point.y >= rect.origin.y
        && point.x < rect.origin.x + rect.size.width
        && point.y < rect.origin.y + rect.size.height
}

#[cfg(test)]
mod tests {
    use scrozz_core::{LogicalPoint, ScaleFactor, ScrollDriver, ScrollGesture};

    use super::CgEventScrollDriver;

    #[test]
    fn pixel_scrolls_expose_a_physical_alignment_prior() {
        let driver = CgEventScrollDriver::new();
        assert_eq!(
            driver.expected_physical_delta(
                &ScrollGesture::down(LogicalPoint::new(10.0, 20.0), 120.0),
                ScaleFactor::new(1.5),
            ),
            Some(180)
        );
    }

    #[test]
    #[ignore = "posts real Quartz wheel events; run only with SCROZZ_NATIVE_SCROLL_SMOKE=1"]
    fn opt_in_native_scroll_post_smoke() {
        if std::env::var("SCROZZ_NATIVE_SCROLL_SMOKE").as_deref() != Ok("1") {
            eprintln!("set SCROZZ_NATIVE_SCROLL_SMOKE=1 to post the native smoke events");
            return;
        }
        if !objc2_core_graphics::CGPreflightPostEventAccess() {
            eprintln!(
                "Accessibility event-post access is not already granted; skipping without prompting"
            );
            return;
        }

        let offscreen = LogicalPoint::new(-10_000.0, -10_000.0);
        let mut driver = CgEventScrollDriver::new();
        driver.prepare().expect("Accessibility event-post grant");
        let pid = i32::try_from(std::process::id()).expect("process id fits pid_t");
        CgEventScrollDriver::post(pid, &ScrollGesture::down(offscreen, 1.0))
            .expect("vertical Quartz wheel event");
        CgEventScrollDriver::post(pid, &ScrollGesture::right(offscreen, 1.0))
            .expect("horizontal Quartz wheel event");
    }
}
