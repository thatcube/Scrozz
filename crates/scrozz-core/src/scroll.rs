//! Driving another application's scroll position.
//!
//! Scrolling capture needs the target to move between frames, and there are only
//! two ways that happens: Scrozz synthesises a wheel event into a window it does
//! not own, or the user scrolls by hand while Scrozz watches. Both are modelled
//! here, because on at least one supported desktop the first is impossible and
//! pretending otherwise would produce a feature that silently captures the same
//! screenful eight times.
//!
//! # Why synthesis is a permission, not an API call
//!
//! Every platform treats "send input to a window belonging to someone else" as
//! privileged, and each treats it differently:
//!
//! - **macOS** requires the Accessibility grant. Without it `CGEventPost`
//!   succeeds and does nothing, which is worse than failing.
//! - **Windows** allows it outright, except into a process running at higher
//!   integrity, where UIPI drops the event silently.
//! - **X11** allows it through XTEST, which is available on essentially every
//!   server and needs no grant.
//! - **Wayland** forbids it entirely except through the `RemoteDesktop` portal,
//!   which GNOME and KDE implement and wlroots does not.
//!
//! Per D15 the grant is requested at [`ScrollDriver::prepare`] — the moment
//! scrolling capture is first used — never at launch. Per D8 the compositors
//! that cannot do it at all report [`crate::Error::Unsupported`] with the reason
//! and the alternative, rather than appearing broken.

use crate::geometry::{LogicalPoint, ScaleFactor};

/// The direction content is gathered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ScrollAxis {
    /// Down the page. What "scrolling capture" means to almost everyone.
    #[default]
    Vertical,
    /// Across the page, for wide tables and timelines.
    Horizontal,
}

/// How, or whether, this platform can move a foreign window's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScrollSynthesis {
    /// Scrozz can post wheel events into the target itself.
    Automatic,
    /// Only the user can scroll; Scrozz captures while they do.
    ///
    /// Carries the reason, which is shown verbatim. "Your compositor does not
    /// implement the RemoteDesktop portal, so Scrozz cannot scroll for you —
    /// scroll and Scrozz will follow" is a usable app. A spinner that never
    /// advances is not.
    Manual {
        /// Why automation is unavailable here, in the user's terms.
        why: String,
    },
}

impl ScrollSynthesis {
    /// Whether Scrozz can drive the scroll itself.
    #[must_use]
    pub const fn is_automatic(&self) -> bool {
        matches!(self, Self::Automatic)
    }
}

/// What a [`ScrollDriver`] can do before anything is attempted.
///
/// Queried, never assumed — the hard API rule D8 imposes on the capture layer
/// applies with more force here, because the same Linux build serves a
/// compositor that can synthesise input and one that cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollCapabilities {
    /// Whether Scrozz can scroll the target itself.
    pub synthesis: ScrollSynthesis,
    /// Whether [`ScrollDriver::prepare`] will ask the user for a grant.
    ///
    /// Lets a caller warn before the system dialog appears, which is the
    /// difference between an expected prompt and a startling one.
    pub requires_permission: bool,
    /// Axes this driver can drive.
    pub axes: &'static [ScrollAxis],
}

impl ScrollCapabilities {
    /// A driver that cannot synthesise input, for the stated reason.
    #[must_use]
    pub fn manual(why: impl Into<String>) -> Self {
        Self {
            synthesis: ScrollSynthesis::Manual { why: why.into() },
            requires_permission: false,
            axes: &[],
        }
    }

    /// A driver that can post wheel events on both axes.
    #[must_use]
    pub const fn automatic(requires_permission: bool) -> Self {
        Self {
            synthesis: ScrollSynthesis::Automatic,
            requires_permission,
            axes: &[ScrollAxis::Vertical, ScrollAxis::Horizontal],
        }
    }

    /// Whether this driver can move the target without the user.
    #[must_use]
    pub const fn is_automatic(&self) -> bool {
        self.synthesis.is_automatic()
    }
}

/// One scroll nudge to deliver into a foreign window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollGesture {
    /// Which way to move.
    pub axis: ScrollAxis,
    /// Where the wheel event lands, in the global logical desktop.
    ///
    /// Wheel events are routed by pointer position on every platform, so this is
    /// not decoration: aimed at the wrong point, the event scrolls whatever is
    /// under the pointer instead of the region being captured. Callers aim it at
    /// the centre of the target.
    pub at: LogicalPoint,
    /// How far to scroll, in logical points.
    ///
    /// Positive moves the viewport *down* the document, which is what makes
    /// content move *up* the screen. Sign errors here are why a scrolling
    /// capture sometimes walks backwards off the top of a page, so the
    /// convention is stated in one place and every driver converts from it.
    pub amount: f64,
}

impl ScrollGesture {
    /// A downward scroll of `amount` logical points at `at`.
    #[must_use]
    pub const fn down(at: LogicalPoint, amount: f64) -> Self {
        Self {
            axis: ScrollAxis::Vertical,
            at,
            amount,
        }
    }

    /// Whether this gesture asks for no movement at all.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        !self.amount.is_finite() || self.amount == 0.0
    }
}

/// Something that can move a foreign window's scroll position.
///
/// Implementations are per platform and live in `scrozz-capture`, which already
/// owns the `unsafe` boundary. [`ManualScrollDriver`] is the portable one: it
/// synthesises nothing and exists so the scrolling-capture session has the same
/// shape whether or not automation is available.
pub trait ScrollDriver: Send {
    /// What this driver can do, before anything is attempted.
    fn capabilities(&self) -> ScrollCapabilities;

    /// Predicts the resulting content displacement in physical pixels.
    ///
    /// Most native wheel APIs cannot make this promise: applications translate
    /// wheel notches through their own line height and scroll settings. Drivers
    /// return `Some` only when their input unit is a logical pixel, allowing the
    /// stitcher to use the value as a prior without turning a wheel delta into a
    /// false seam.
    fn expected_physical_delta(
        &self,
        _gesture: &ScrollGesture,
        _frame_scale: ScaleFactor,
    ) -> Option<u32> {
        None
    }

    /// Acquires whatever grant or session synthesis needs.
    ///
    /// Called once, at the moment scrolling capture is first used — never at
    /// launch (D15). Idempotent: a session may prepare a driver that is already
    /// prepared.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::PermissionDenied`] if the user declined or has
    /// not yet granted the platform's input grant, or
    /// [`crate::Error::Unsupported`] if this desktop has no synthesis path at
    /// all. Both are ordinary outcomes: the caller falls back to asking the user
    /// to scroll by hand.
    fn prepare(&mut self) -> crate::Result<()>;

    /// Delivers one scroll nudge.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Unsupported`] on a driver that cannot synthesise
    /// input, [`crate::Error::PermissionDenied`] if the grant was revoked
    /// mid-session, or [`crate::Error::Platform`] if the event could not be
    /// posted.
    fn scroll(&mut self, gesture: &ScrollGesture) -> crate::Result<()>;

    /// Human-readable driver name for diagnostics, e.g. "CGEvent".
    fn name(&self) -> &str;
}

/// The driver for desktops where only the user can scroll.
///
/// Not a stub and not a failure mode: on wlroots compositors this is the
/// *correct* driver, and the resulting flow — "scroll; Scrozz follows; press
/// Escape when you are done" — is a working feature rather than a degraded one.
#[derive(Debug, Clone)]
pub struct ManualScrollDriver {
    why: String,
}

impl ManualScrollDriver {
    /// A manual driver that explains itself with `why`.
    #[must_use]
    pub fn new(why: impl Into<String>) -> Self {
        Self { why: why.into() }
    }
}

impl ScrollDriver for ManualScrollDriver {
    fn capabilities(&self) -> ScrollCapabilities {
        ScrollCapabilities::manual(self.why.clone())
    }

    fn prepare(&mut self) -> crate::Result<()> {
        // Nothing to acquire. Deliberately `Ok`: a manual session is a supported
        // way to take a scrolling capture, so refusing here would turn a working
        // flow into an error the user cannot act on.
        Ok(())
    }

    fn scroll(&mut self, _gesture: &ScrollGesture) -> crate::Result<()> {
        Err(crate::Error::Unsupported {
            what: "scrolling the target automatically".to_string(),
            why: self.why.clone(),
        })
    }

    fn name(&self) -> &str {
        "manual"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manual_driver_prepares_but_refuses_to_scroll() {
        let mut driver = ManualScrollDriver::new("this compositor has no RemoteDesktop portal");
        assert!(!driver.capabilities().is_automatic());
        assert!(
            driver.prepare().is_ok(),
            "a manual session is supported, not broken"
        );

        let err = driver
            .scroll(&ScrollGesture::down(LogicalPoint::new(10.0, 10.0), 100.0))
            .expect_err("a manual driver cannot synthesise");
        assert!(matches!(err, crate::Error::Unsupported { .. }));
        assert!(err.to_string().contains("RemoteDesktop"), "{err}");
    }

    #[test]
    fn the_reason_travels_with_the_capability() {
        let caps = ScrollCapabilities::manual("sway does not implement it");
        match caps.synthesis {
            ScrollSynthesis::Manual { why } => assert_eq!(why, "sway does not implement it"),
            ScrollSynthesis::Automatic => panic!("asked for manual"),
        }
    }

    #[test]
    fn an_automatic_driver_advertises_both_axes() {
        let caps = ScrollCapabilities::automatic(true);
        assert!(caps.is_automatic());
        assert!(caps.requires_permission);
        assert_eq!(caps.axes, [ScrollAxis::Vertical, ScrollAxis::Horizontal]);
    }

    #[test]
    fn a_zero_or_nonfinite_gesture_is_a_noop() {
        let at = LogicalPoint::new(0.0, 0.0);
        assert!(ScrollGesture::down(at, 0.0).is_noop());
        assert!(ScrollGesture::down(at, f64::NAN).is_noop());
        assert!(!ScrollGesture::down(at, -40.0).is_noop());
    }
}
