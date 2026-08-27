//! Accessibility-gated scroll synthesis through Quartz events.

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventTapLocation, CGPreflightPostEventAccess, CGRequestPostEventAccess,
    CGScrollEventUnit,
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

        let (wheel1, wheel2) = scroll_units::macos_deltas(gesture.axis, gesture.amount);
        let event =
            CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, wheel1, wheel2, 0)
                .ok_or_else(|| {
                    Error::Platform("CGEventCreateScrollWheelEvent2 returned null".into())
                })?;

        CGEvent::set_location(Some(&event), CGPoint::new(gesture.at.x, gesture.at.y));
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }

    fn name(&self) -> &str {
        "CGEvent"
    }
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
}
