//! Enumerating displays, and getting their scale factor right.
//!
//! CoreGraphics is the primary source here rather than AppKit, for two reasons.
//! It is callable from any thread — `NSScreen` is main-thread-only, and a
//! capture backend cannot demand the main thread — and it works before any
//! `NSApplication` exists, which matters for a CLI or a daemon.
//!
//! AppKit is still consulted when the caller happens to be on the main thread,
//! purely for two things CoreGraphics cannot supply: the display's real name,
//! and the work area left over once the menu bar and Dock are subtracted.

use objc2_core_foundation::CGRect;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsBuiltin,
    CGDisplayIsMain, CGDisplayMode, CGError, CGGetActiveDisplayList, CGMainDisplayID,
};
use scrozz_core::{
    Display, DisplayId, Error, LogicalPoint, LogicalRect, LogicalSize, Result, ScaleFactor,
};

/// More displays than any Mac supports; the list is fetched in one shot.
const MAX_DISPLAYS: u32 = 32;

/// Every active display, in the order CoreGraphics reports them.
pub(crate) fn displays() -> Result<Vec<Display>> {
    let ids = active_display_ids()?;
    let names = super::appkit::display_names_and_work_areas();

    Ok(ids
        .into_iter()
        .map(|id| {
            let bounds = logical_bounds(id);
            let (name, work_area) = names.get(&id).cloned().unwrap_or_default();

            Display {
                id: display_id(id),
                name: name.unwrap_or_else(|| fallback_name(id)),
                bounds,
                work_area: work_area.unwrap_or(bounds),
                scale: scale_factor(id),
                is_primary: CGDisplayIsMain(id),
            }
        })
        .collect())
}

/// The display the user is currently pointing at.
///
/// "Active" is taken to mean the display under the mouse, which is what the
/// user is looking at when they reach for a screenshot shortcut. The keyboard
/// focus alternative would need AppKit and the main thread, and would still be
/// wrong for a menu-bar-driven capture.
pub(crate) fn active_display() -> Result<Display> {
    let all = displays()?;
    if all.is_empty() {
        return Err(Error::Unsupported {
            what: "finding the active display".to_owned(),
            why: "no displays are attached".to_owned(),
        });
    }

    if let Some(point) = super::appkit::mouse_location()
        && let Some(display) = all.iter().find(|display| contains(display.bounds, point))
    {
        return Ok(display.clone());
    }

    let main = display_id(CGMainDisplayID());
    Ok(all
        .iter()
        .find(|display| display.id == main)
        .or_else(|| all.iter().find(|display| display.is_primary))
        .unwrap_or(&all[0])
        .clone())
}

/// The CoreGraphics ID behind one of our opaque [`DisplayId`]s.
pub(crate) fn parse_display_id(id: &DisplayId) -> Option<CGDirectDisplayID> {
    id.0.parse().ok()
}

pub(crate) fn display_id(id: CGDirectDisplayID) -> DisplayId {
    DisplayId(id.to_string())
}

/// The bounds of a display in the global, top-left-origin coordinate space.
///
/// This needs no flipping: `CGDisplayBounds` already uses a downward y-axis
/// with the primary display's top-left at the origin, which is exactly what
/// [`LogicalRect`] means.
pub(crate) fn logical_bounds(id: CGDirectDisplayID) -> LogicalRect {
    from_cg_rect(CGDisplayBounds(id))
}

pub(crate) fn from_cg_rect(rect: CGRect) -> LogicalRect {
    LogicalRect::new(
        LogicalPoint::new(rect.origin.x, rect.origin.y),
        LogicalSize::new(rect.size.width, rect.size.height),
    )
}

/// The inverse, for handing a rectangle back to CoreGraphics.
pub(crate) fn to_cg_rect(rect: LogicalRect) -> CGRect {
    CGRect::new(
        objc2_core_foundation::CGPoint::new(rect.origin.x, rect.origin.y),
        objc2_core_foundation::CGSize::new(rect.size.width, rect.size.height),
    )
}

/// The display's true backing-scale factor.
///
/// Computed as the current mode's pixel width over its point width, which is
/// the ratio that actually governs how many pixels a capture comes back with.
/// Reading `NSScreen.backingScaleFactor` would agree on the common cases and
/// disagree on the interesting one: a Retina panel driven at a "More Space"
/// scaled resolution renders to a backing store larger than its point size by a
/// non-integral ratio, and it is that ratio — not a rounded 2.0 — that decides
/// the captured pixel count.
///
/// Falls back to 1.0 when the mode cannot be read, which is honest: an unscaled
/// capture of unknown provenance beats a guessed-at 2× that doubles nothing.
pub(crate) fn scale_factor(id: CGDirectDisplayID) -> ScaleFactor {
    let Some(mode) = CGDisplayCopyDisplayMode(id) else {
        return ScaleFactor::IDENTITY;
    };

    let points = CGDisplayMode::width(Some(&mode));
    let pixels = CGDisplayMode::pixel_width(Some(&mode));
    if points == 0 || pixels == 0 {
        return ScaleFactor::IDENTITY;
    }

    scale_from_ratio(pixels as f64 / points as f64)
}

/// Builds a [`ScaleFactor`], refusing anything that would panic or mislead.
///
/// `ScaleFactor::new` panics on non-finite or non-positive input, so a bad
/// reading from the window server must not reach it. An implausible scale is
/// treated the same as an unreadable one.
pub(crate) fn scale_from_ratio(ratio: f64) -> ScaleFactor {
    if ratio.is_finite() && (0.1..=16.0).contains(&ratio) {
        ScaleFactor::new(ratio)
    } else {
        ScaleFactor::IDENTITY
    }
}

fn fallback_name(id: CGDirectDisplayID) -> String {
    if CGDisplayIsBuiltin(id) {
        "Built-in Display".to_owned()
    } else {
        format!("Display {id}")
    }
}

pub(crate) fn contains(rect: LogicalRect, (x, y): (f64, f64)) -> bool {
    x >= rect.origin.x
        && y >= rect.origin.y
        && x < rect.origin.x + rect.size.width
        && y < rect.origin.y + rect.size.height
}

fn active_display_ids() -> Result<Vec<CGDirectDisplayID>> {
    let mut ids = [0 as CGDirectDisplayID; MAX_DISPLAYS as usize];
    let mut count: u32 = 0;

    // SAFETY: both pointers address live, correctly sized local storage, and
    // `MAX_DISPLAYS` matches the array's length.
    let status = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS, ids.as_mut_ptr(), &raw mut count) };
    if status != CGError::Success {
        return Err(Error::Platform(format!(
            "CGGetActiveDisplayList failed with code {}",
            status.0
        )));
    }

    let count = (count as usize).min(ids.len());
    Ok(ids[..count].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implausible_scales_fall_back_to_identity_rather_than_panicking() {
        for ratio in [0.0, -2.0, f64::NAN, f64::INFINITY, 1e9] {
            assert_eq!(scale_from_ratio(ratio), ScaleFactor::IDENTITY);
        }
    }

    #[test]
    fn plausible_scales_are_preserved_exactly() {
        assert_eq!(scale_from_ratio(2.0).get(), 2.0);
        assert_eq!(scale_from_ratio(1.5).get(), 1.5);
    }

    #[test]
    fn display_ids_round_trip() {
        let id = display_id(42);
        assert_eq!(parse_display_id(&id), Some(42));
    }

    #[test]
    fn a_non_numeric_display_id_is_rejected_rather_than_guessed() {
        assert_eq!(parse_display_id(&DisplayId("main".to_owned())), None);
    }

    #[test]
    fn cg_rects_map_to_logical_rects_without_flipping() {
        let rect = CGRect::new(
            objc2_core_foundation::CGPoint::new(100.0, 200.0),
            objc2_core_foundation::CGSize::new(300.0, 400.0),
        );
        let logical = from_cg_rect(rect);
        assert_eq!(logical.origin.x, 100.0);
        assert_eq!(logical.origin.y, 200.0);
        assert_eq!(logical.size.width, 300.0);
        assert_eq!(logical.size.height, 400.0);
    }

    #[test]
    fn containment_excludes_the_far_edges_so_adjacent_displays_do_not_overlap() {
        let left = LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(100.0, 100.0));
        let right = LogicalRect::new(
            LogicalPoint::new(100.0, 0.0),
            LogicalSize::new(100.0, 100.0),
        );

        assert!(contains(left, (99.9, 50.0)));
        assert!(!contains(left, (100.0, 50.0)));
        assert!(contains(right, (100.0, 50.0)));
    }
}
