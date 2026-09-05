//! Accessibility-gated scroll synthesis through Quartz events.

use std::time::Duration;

use objc2_app_kit::NSApplication;
use objc2_core_foundation::{CFDictionary, CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGPreflightPostEventAccess,
    CGRectMakeWithDictionaryRepresentation, CGRequestPostEventAccess, CGScrollEventUnit,
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGWindowBounds,
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
pub(crate) struct CgEventScrollDriver {
    target: Option<NativeWindowIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeWindowIdentity {
    window: u32,
    process: i32,
}

impl CgEventScrollDriver {
    pub(crate) const fn new() -> Self {
        Self { target: None }
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

    fn ensure_selected_window_owns_point(
        gesture: &ScrollGesture,
        expected: Option<NativeWindowIdentity>,
    ) -> Result<NativeWindowIdentity> {
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
        ensure_window_geometry(
            gesture.window_bounds,
            super::display::from_cg_rect(unsafe { selected_window.frame() }),
        )?;
        let current_pid = i32::try_from(std::process::id()).map_err(|_| {
            Error::Platform("the Scrozz process id does not fit macOS pid_t".into())
        })?;
        if selected_pid == current_pid {
            return Err(Error::Unsupported {
                what: "automatically scrolling Scrozz itself".into(),
                why: "choose a window belonging to another application".into(),
            });
        }
        let selected_identity = NativeWindowIdentity {
            window: selected_id,
            process: selected_pid,
        };
        if let Some(owner) = gesture.owner_pid {
            let process = i32::try_from(owner).map_err(|_| {
                Error::InvalidRequest("selected window process does not fit macOS pid_t".into())
            })?;
            ensure_same_window_instance(
                Some(NativeWindowIdentity {
                    window: selected_id,
                    process,
                }),
                selected_identity,
                &selected.0,
            )?;
        }
        ensure_same_window_instance(expected, selected_identity, &selected.0)?;

        let mut transparent = None;
        for window_id in ordered {
            let frame = match current_window_frame(window_id) {
                CurrentWindowFrame::Gone => continue,
                CurrentWindowFrame::Unverified => {
                    return Err(Error::TargetGone(format!(
                        "Scrozz could not verify window {window_id} ahead of the selected target"
                    )));
                }
                CurrentWindowFrame::Frame(frame) => frame,
            };
            if !contains(frame, gesture.at) {
                continue;
            }
            let window = require_shareable_window(
                windows
                    .iter()
                    .find(|window| unsafe { window.windowID() == window_id }),
                window_id,
            )?;
            let owner = unsafe {
                window
                    .owningApplication()
                    .map(|application| application.processID())
            };
            if owner == Some(current_pid) {
                let transparent = match &transparent {
                    Some(transparent) => transparent,
                    None => transparent.insert(transparent_own_windows()?),
                };
                // Only natively mouse-transparent Scrozz windows may be
                // skipped. Settings, editors, and detached controls block input.
                if is_transparent_own_window(owner, window_id, current_pid, transparent.as_slice())
                {
                    continue;
                }
            }
            if window_id != selected_id {
                return Err(Error::TargetGone(format!(
                    "window {} no longer owns the scroll point; it is covered by window \
                     {window_id}",
                    selected.0
                )));
            }
            ensure_window_geometry(gesture.window_bounds, frame)?;
            return Ok(selected_identity);
        }
        Err(Error::TargetGone(format!(
            "window {} is no longer visible at the selected scroll point",
            selected.0
        )))
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

    fn deliver(&mut self, gesture: &ScrollGesture) -> Result<ScrollDelivery> {
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
        let identity = Self::ensure_selected_window_owns_point(&at_pointer, self.target)?;
        let event = Self::event(&at_pointer)?;
        let latest = current_pointer()?;
        if latest != point || !contains(area, latest) {
            return Ok(ScrollDelivery::PointerOutside);
        }
        let identity = Self::ensure_selected_window_owns_point(&at_pointer, Some(identity))?;
        let latest = current_pointer()?;
        if latest != point || !contains(area, latest) {
            return Ok(ScrollDelivery::PointerOutside);
        }
        Self::ensure_trusted()?;
        // Browser scroll handlers need WindowServer's normal wheel routing.
        // Never warp the cursor or inject while it is outside the selected area.
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        self.target = Some(identity);
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
        match self.deliver(gesture)? {
            ScrollDelivery::Submitted => Ok(()),
            ScrollDelivery::PointerOutside => Err(Error::InvalidRequest(
                "move the pointer inside the capture area to scroll automatically".into(),
            )),
        }
    }

    fn try_scroll(&mut self, gesture: &ScrollGesture) -> Result<ScrollDelivery> {
        self.deliver(gesture)
    }

    fn name(&self) -> &str {
        "CGEvent"
    }
}

enum CurrentWindowFrame {
    Gone,
    Frame(LogicalRect),
    Unverified,
}

fn current_window_frame(window: u32) -> CurrentWindowFrame {
    // Query the ID directly. CGWindowListCreateDescriptionFromArray expects
    // raw ID pointer slots, not CFNumber objects.
    let Some(windows) =
        CGWindowListCopyWindowInfo(CGWindowListOption::OptionIncludingWindow, window)
    else {
        return CurrentWindowFrame::Unverified;
    };
    if windows.is_empty() {
        return CurrentWindowFrame::Gone;
    }
    if windows.count() != 1 {
        return CurrentWindowFrame::Unverified;
    }

    // SAFETY: this API documents each returned array element as a CFDictionary.
    let description = unsafe { &*windows.value_at_index(0).cast::<CFDictionary>() };
    // SAFETY: kCGWindowBounds is an immortal framework constant and the
    // dictionary has the documented CGWindow description shape.
    let bounds = unsafe { description.value(std::ptr::from_ref(kCGWindowBounds).cast()) };
    if bounds.is_null() {
        return CurrentWindowFrame::Unverified;
    }
    // SAFETY: the value for kCGWindowBounds is a CGRect dictionary.
    let bounds = unsafe { &*bounds.cast::<CFDictionary>() };
    let mut frame = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0));
    // SAFETY: both pointers are valid for this call.
    if !unsafe { CGRectMakeWithDictionaryRepresentation(Some(bounds), &mut frame) } {
        return CurrentWindowFrame::Unverified;
    }
    let frame = super::display::from_cg_rect(frame);
    if valid_rect(frame) {
        CurrentWindowFrame::Frame(frame)
    } else {
        CurrentWindowFrame::Unverified
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

fn ensure_same_window_instance(
    expected: Option<NativeWindowIdentity>,
    actual: NativeWindowIdentity,
    selected: &str,
) -> Result<()> {
    if expected.is_some_and(|expected| expected != actual) {
        return Err(Error::TargetGone(format!(
            "window {selected} was replaced by a different native window or process"
        )));
    }
    Ok(())
}

fn ensure_window_geometry(expected: Option<LogicalRect>, actual: LogicalRect) -> Result<()> {
    if expected.is_some_and(|expected| expected != actual) {
        return Err(Error::TargetGone(
            "the selected window moved or resized; redraw the scrolling area before resuming Auto"
                .into(),
        ));
    }
    Ok(())
}

fn require_shareable_window<T>(window: Option<T>, window_id: u32) -> Result<T> {
    window.ok_or_else(|| {
        Error::TargetGone(format!(
            "an unverified window {window_id} covers the selected scroll point"
        ))
    })
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

fn valid_rect(rect: LogicalRect) -> bool {
    scroll_units::finite_point(rect.origin)
        && rect.size.width.is_finite()
        && rect.size.height.is_finite()
        && rect.size.width >= 0.0
        && rect.size.height >= 0.0
        && scroll_units::finite_point(LogicalPoint::new(
            rect.origin.x + rect.size.width,
            rect.origin.y + rect.size.height,
        ))
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

    #[test]
    fn a_window_id_cannot_change_process_during_automatic_scrolling() {
        let selected = super::NativeWindowIdentity {
            window: 42,
            process: 100,
        };
        assert!(super::ensure_same_window_instance(None, selected, "42").is_ok());
        assert!(super::ensure_same_window_instance(Some(selected), selected, "42").is_ok());
        assert!(matches!(
            super::ensure_same_window_instance(
                Some(selected),
                super::NativeWindowIdentity {
                    window: 42,
                    process: 101,
                },
                "42",
            ),
            Err(scrozz_core::Error::TargetGone(_))
        ));
    }

    #[test]
    fn a_core_graphics_blocker_missing_from_capture_content_fails_closed() {
        assert_eq!(super::require_shareable_window(Some(7), 42).unwrap(), 7);
        assert!(matches!(
            super::require_shareable_window::<u32>(None, 42),
            Err(scrozz_core::Error::TargetGone(_))
        ));
    }

    #[test]
    fn native_input_refuses_window_translation_and_resize() {
        let original = scrozz_core::LogicalRect::new(
            LogicalPoint::new(100.0, 200.0),
            scrozz_core::LogicalSize::new(500.0, 300.0),
        );
        assert!(super::ensure_window_geometry(Some(original), original).is_ok());
        for changed in [
            scrozz_core::LogicalRect::new(LogicalPoint::new(110.0, 200.0), original.size),
            scrozz_core::LogicalRect::new(
                original.origin,
                scrozz_core::LogicalSize::new(600.0, 300.0),
            ),
        ] {
            assert!(super::ensure_window_geometry(Some(original), changed).is_err());
        }
        let empty =
            scrozz_core::LogicalRect::new(original.origin, scrozz_core::LogicalSize::new(0.0, 0.0));
        assert!(super::valid_rect(empty));
        assert!(!super::contains(empty, original.origin));
    }

    #[test]
    #[ignore = "requires WindowServer; reads window geometry only, never pixels or input"]
    fn native_window_geometry_query_accepts_window_server_ids() {
        let ids = super::super::window::on_screen_window_ids_front_to_back().expect("WindowServer");
        assert!(
            ids.into_iter().any(|id| matches!(
                super::current_window_frame(id),
                super::CurrentWindowFrame::Frame(_)
            )),
            "at least one on-screen window must have readable geometry"
        );
    }
}
