//! Accessibility-gated scroll synthesis through Quartz events.

use std::time::Duration;

use objc2_app_kit::NSApplication;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGPreflightPostEventAccess,
    CGRequestPostEventAccess, CGScrollEventUnit,
};
use objc2_foundation::MainThreadMarker;
use scrozz_core::{
    Error, LogicalPoint, LogicalRect, Result, ScaleFactor, ScrollCapabilities, ScrollDelivery,
    ScrollDriver, ScrollGesture,
};

use crate::scroll_units;

const CAPABILITY: &str = "Accessibility control for scrolling other applications";
const REMEDY: &str =
    "System Settings → Privacy & Security → Accessibility: switch Scrozz on, then retry";
const MAX_PIXEL_STEP: f64 = 48.0;

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

    fn ensure_selected_window_owns_point(gesture: &ScrollGesture) -> Result<()> {
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
        if selected_pid == current_pid {
            return Err(Error::Unsupported {
                what: "automatically scrolling Scrozz itself".into(),
                why: "choose a window belonging to another application".into(),
            });
        }
        let transparent = if windows.iter().any(|window| unsafe {
            window.isOnScreen()
                && contains(super::display::from_cg_rect(window.frame()), gesture.at)
                && window
                    .owningApplication()
                    .is_some_and(|app| app.processID() == current_pid)
        }) {
            transparent_own_windows()?
        } else {
            Vec::new()
        };
        let frontmost = ordered.into_iter().find_map(|window_id| {
            windows
                .iter()
                .find(|window| unsafe { window.windowID() == window_id })
                // Only natively mouse-transparent Scrozz windows may be
                // skipped. Settings, editors, and detached controls block input.
                .filter(|window| unsafe {
                    !is_transparent_own_window(
                        window
                            .owningApplication()
                            .map(|application| application.processID()),
                        window.windowID(),
                        current_pid,
                        &transparent,
                    )
                })
                .filter(|window| unsafe {
                    window.isOnScreen()
                        && contains(super::display::from_cg_rect(window.frame()), gesture.at)
                })
        });
        match frontmost {
            Some(window) if unsafe { window.windowID() } == selected_id => Ok(()),
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

    fn event(gesture: &ScrollGesture) -> Result<CFRetained<CGEvent>> {
        let limit = gesture.area.map_or(MAX_PIXEL_STEP, |area| {
            let extent = match gesture.axis {
                scrozz_core::ScrollAxis::Vertical => area.size.height,
                scrozz_core::ScrollAxis::Horizontal => area.size.width,
            };
            (extent * 0.2).min(MAX_PIXEL_STEP)
        });
        let amount = gesture.amount.signum() * gesture.amount.abs().min(limit);
        let (wheel1, wheel2) = scroll_units::macos_deltas(gesture.axis, amount);
        let event =
            CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, wheel1, wheel2, 0)
                .ok_or_else(|| {
                    Error::Platform("CGEventCreateScrollWheelEvent2 returned null".into())
                })?;

        CGEvent::set_location(Some(&event), CGPoint::new(gesture.at.x, gesture.at.y));
        CGEvent::set_flags(Some(&event), CGEventFlags::empty());
        Ok(event)
    }

    fn deliver(gesture: &ScrollGesture) -> Result<ScrollDelivery> {
        if gesture.is_noop() {
            return Ok(ScrollDelivery::Submitted);
        }
        let area = gesture
            .area
            .filter(|area| {
                !area.is_empty()
                    && scroll_units::finite_point(area.origin)
                    && area.size.width.is_finite()
                    && area.size.height.is_finite()
                    && scroll_units::finite_point(LogicalPoint::new(
                        area.origin.x + area.size.width,
                        area.origin.y + area.size.height,
                    ))
            })
            .ok_or_else(|| {
                Error::InvalidRequest(
                    "macOS automatic scrolling requires a bounded capture area".into(),
                )
            })?;
        Self::ensure_trusted()?;
        let point = current_pointer()?;
        if !contains(area, point) {
            return Ok(ScrollDelivery::PointerOutside);
        }
        let mut at_pointer = gesture.clone();
        at_pointer.at = point;
        Self::ensure_selected_window_owns_point(&at_pointer)?;
        let event = Self::event(&at_pointer)?;
        let latest = current_pointer()?;
        if latest != point || !contains(area, latest) {
            return Ok(ScrollDelivery::PointerOutside);
        }
        // Browser scroll handlers need WindowServer's normal wheel routing.
        // Never warp the cursor or inject while it is outside the selected area.
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(ScrollDelivery::Submitted)
    }
}

impl ScrollDriver for CgEventScrollDriver {
    fn capabilities(&self) -> ScrollCapabilities {
        ScrollCapabilities::automatic(true)
    }

    fn expected_physical_delta(
        &self,
        gesture: &ScrollGesture,
        _frame_scale: ScaleFactor,
    ) -> Option<u32> {
        let _ = gesture;
        // View zoom and scroll snapping can change the observed displacement.
        // Captured pixels, not a synthetic delta, decide the seam.
        None
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
        match Self::deliver(gesture)? {
            ScrollDelivery::Submitted => Ok(()),
            ScrollDelivery::PointerOutside => Err(Error::InvalidRequest(
                "move the pointer inside the capture area to scroll automatically".into(),
            )),
        }
    }

    fn try_scroll(&mut self, gesture: &ScrollGesture) -> Result<ScrollDelivery> {
        Self::deliver(gesture)
    }

    fn name(&self) -> &str {
        "CGEvent"
    }
}

fn current_pointer() -> Result<LogicalPoint> {
    let (x, y) = super::appkit::mouse_location().ok_or_else(|| {
        Error::Platform("could not read the mouse position for Auto scrolling".into())
    })?;
    let point = LogicalPoint::new(x, y);
    if !scroll_units::finite_point(point) {
        return Err(Error::Platform(
            "macOS returned an invalid mouse position".into(),
        ));
    }
    Ok(point)
}

fn is_transparent_own_window(
    owner: Option<i32>,
    window: u32,
    current: i32,
    transparent: &[u32],
) -> bool {
    owner == Some(current) && transparent.contains(&window)
}

fn transparent_own_windows() -> Result<Vec<u32>> {
    fn read(mtm: MainThreadMarker) -> Vec<u32> {
        NSApplication::sharedApplication(mtm)
            .windows()
            .iter()
            .filter(|window| window.ignoresMouseEvents())
            .filter_map(|window| u32::try_from(window.windowNumber()).ok())
            .collect()
    }
    if let Some(mtm) = MainThreadMarker::new() {
        return Ok(read(mtm));
    }
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    dispatch2::DispatchQueue::main().exec_async(move || {
        let result = MainThreadMarker::new()
            .map(read)
            .ok_or_else(|| Error::Platform("could not verify Scrozz window input state".into()));
        let _ = send.send(result);
    });
    receive
        .recv_timeout(Duration::from_millis(150))
        .map_err(|_| {
            Error::Platform("Scrozz could not confirm safe pointer passthrough in time".into())
        })?
}

fn contains(rect: LogicalRect, point: LogicalPoint) -> bool {
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
    fn wheel_payloads_preserve_direction_without_keyboard_modifiers() {
        use objc2_app_kit::{NSEvent, NSEventType};
        use objc2_core_graphics::{CGEvent, CGEventFlags, CGScrollEventUnit};
        let point = LogicalPoint::new(300.0, 400.0);
        for gesture in [
            ScrollGesture::down(point, 120.0),
            ScrollGesture::up(point, 120.0),
            ScrollGesture::right(point, 120.0),
            ScrollGesture::left(point, 120.0),
        ] {
            let event = CgEventScrollDriver::event(&gesture).unwrap();
            let native = NSEvent::eventWithCGEvent(&event).unwrap();
            assert_eq!(native.r#type(), NSEventType::ScrollWheel);
            let (y, x) =
                crate::scroll_units::macos_deltas(gesture.axis, gesture.amount.signum() * 48.0);
            let legacy =
                CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, y, x, 0)
                    .unwrap();
            let legacy_native = NSEvent::eventWithCGEvent(&legacy).unwrap();
            assert_eq!(native.scrollingDeltaX(), legacy_native.scrollingDeltaX());
            assert_eq!(native.scrollingDeltaY(), legacy_native.scrollingDeltaY());
            assert!(native.hasPreciseScrollingDeltas());
            assert_eq!(
                CGEvent::location(Some(&event)),
                objc2_core_foundation::CGPoint::new(point.x, point.y)
            );
            assert_eq!(CGEvent::flags(Some(&event)), CGEventFlags::empty());
        }
    }

    #[test]
    fn precise_steps_shrink_for_small_capture_areas() {
        let area = scrozz_core::LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            scrozz_core::LogicalSize::new(50.0, 40.0),
        );
        let vertical = CgEventScrollDriver::event(
            &ScrollGesture::down(LogicalPoint::new(20.0, 20.0), 1_000.0).within(area),
        )
        .unwrap();
        let horizontal = CgEventScrollDriver::event(
            &ScrollGesture::right(LogicalPoint::new(20.0, 20.0), 1_000.0).within(area),
        )
        .unwrap();
        assert_eq!(
            objc2_app_kit::NSEvent::eventWithCGEvent(&vertical)
                .unwrap()
                .scrollingDeltaY(),
            -8.0
        );
        assert_eq!(
            objc2_app_kit::NSEvent::eventWithCGEvent(&horizontal)
                .unwrap()
                .scrollingDeltaX(),
            -10.0
        );
    }

    #[test]
    fn scrolls_do_not_invent_a_physical_alignment_prior() {
        let driver = CgEventScrollDriver::new();
        assert_eq!(
            driver.expected_physical_delta(
                &ScrollGesture::down(LogicalPoint::new(10.0, 20.0), 120.0),
                ScaleFactor::new(1.5),
            ),
            None
        );
    }

    #[test]
    fn pointer_must_stay_inside_the_selected_area() {
        let area = scrozz_core::LogicalRect::new(
            LogicalPoint::new(100.0, 200.0),
            scrozz_core::LogicalSize::new(400.0, 300.0),
        );
        assert!(super::contains(area, LogicalPoint::new(300.0, 350.0)));
        for point in [
            LogicalPoint::new(99.0, 350.0),
            LogicalPoint::new(500.0, 350.0),
            LogicalPoint::new(300.0, 199.0),
            LogicalPoint::new(300.0, 500.0),
        ] {
            assert!(!super::contains(area, point));
        }
        assert!(
            CgEventScrollDriver::new()
                .try_scroll(&ScrollGesture::down(LogicalPoint::new(100.0, 100.0), 30.0))
                .is_err(),
            "missing area fails before permission queries or event delivery"
        );
    }

    #[test]
    fn only_proven_transparent_own_windows_are_skipped_during_hit_testing() {
        assert!(super::is_transparent_own_window(Some(10), 42, 10, &[42]));
        assert!(!super::is_transparent_own_window(Some(10), 43, 10, &[42]));
        assert!(!super::is_transparent_own_window(Some(11), 42, 10, &[42]));
        assert!(!super::is_transparent_own_window(None, 42, 10, &[42]));
    }
}
