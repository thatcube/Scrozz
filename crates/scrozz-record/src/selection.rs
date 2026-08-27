//! Pure recording-selection geometry.
//!
//! This module resolves gestures only. It never enumerates windows, reads the
//! pointer, or opens an overlay, so the same inputs always produce the same
//! capture target on every platform.

use scrozz_core::{
    CaptureTarget, DisplayId, Error, LogicalPoint, LogicalRect, LogicalSize, Result, WindowId,
};

/// Default movement, in logical points, that distinguishes a drag from a click.
pub const DEFAULT_DRAG_THRESHOLD: f64 = 3.0;

/// How a selection gesture is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionMode {
    /// A click chooses the hovered window or active display; a drag chooses a
    /// region.
    #[default]
    AllInOne,
    /// Only a non-empty dragged region is accepted.
    Region,
}

/// A positive width-to-height ratio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AspectRatio {
    width: f64,
    height: f64,
}

impl AspectRatio {
    /// Creates a validated aspect ratio.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] unless both terms are finite and
    /// strictly positive.
    pub fn new(width: f64, height: f64) -> Result<Self> {
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Err(Error::InvalidRequest(format!(
                "aspect ratio {width}:{height} must contain two positive finite values"
            )));
        }
        Ok(Self { width, height })
    }

    /// Width divided by height.
    #[must_use]
    pub fn value(self) -> f64 {
        self.width / self.height
    }

    /// The original width term.
    #[must_use]
    pub const fn width(self) -> f64 {
        self.width
    }

    /// The original height term.
    #[must_use]
    pub const fn height(self) -> f64 {
        self.height
    }
}

/// Optional geometric constraints applied to a dragged region.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SelectionConstraints {
    /// Exact logical dimensions. The rectangle may translate at a desktop edge,
    /// but these dimensions are never silently changed.
    pub exact_size: Option<LogicalSize>,
    /// Aspect ratio retained while dragging.
    pub aspect_ratio: Option<AspectRatio>,
}

impl SelectionConstraints {
    /// No exact size and no aspect lock.
    pub const NONE: Self = Self {
        exact_size: None,
        aspect_ratio: None,
    };

    /// An exact-size constraint.
    #[must_use]
    pub fn exact(width: f64, height: f64) -> Self {
        Self {
            exact_size: Some(LogicalSize::new(width, height)),
            aspect_ratio: None,
        }
    }

    /// An aspect-ratio constraint.
    #[must_use]
    pub const fn aspect(aspect_ratio: AspectRatio) -> Self {
        Self {
            exact_size: None,
            aspect_ratio: Some(aspect_ratio),
        }
    }

    /// Validates the constraints, including agreement when both are set.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for empty/non-finite dimensions or an
    /// exact size that conflicts with the aspect lock.
    pub fn validate(self) -> Result<Self> {
        if let Some(size) = self.exact_size {
            if !size.width.is_finite()
                || !size.height.is_finite()
                || size.width <= 0.0
                || size.height <= 0.0
            {
                return Err(Error::InvalidRequest(format!(
                    "exact selection size {}x{} must be finite and non-zero",
                    size.width, size.height
                )));
            }
            if let Some(ratio) = self.aspect_ratio {
                let actual = size.width / size.height;
                if (actual - ratio.value()).abs() > ratio.value() * 1.0e-9 {
                    return Err(Error::InvalidRequest(format!(
                        "exact selection size {}x{} conflicts with aspect ratio {}:{}",
                        size.width,
                        size.height,
                        ratio.width(),
                        ratio.height()
                    )));
                }
            }
        }
        Ok(self)
    }
}

/// All deterministic inputs needed to resolve one pointer gesture.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionGesture {
    /// Where the press began.
    pub start: LogicalPoint,
    /// Where the pointer was released.
    pub end: LogicalPoint,
    /// Window under a click, obtained from real platform enumeration.
    pub hovered_window: Option<WindowId>,
    /// Display used when an all-in-one click has no enumerated window.
    pub active_display: DisplayId,
    /// Union of the available logical desktop.
    pub desktop_bounds: LogicalRect,
    /// Movement required before this is a drag.
    pub drag_threshold: f64,
}

impl SelectionGesture {
    /// Creates a gesture with [`DEFAULT_DRAG_THRESHOLD`].
    #[must_use]
    pub fn new(
        start: LogicalPoint,
        end: LogicalPoint,
        hovered_window: Option<WindowId>,
        active_display: DisplayId,
        desktop_bounds: LogicalRect,
    ) -> Self {
        Self {
            start,
            end,
            hovered_window,
            active_display,
            desktop_bounds,
            drag_threshold: DEFAULT_DRAG_THRESHOLD,
        }
    }

    /// Whether movement crosses the configured click/drag threshold.
    #[must_use]
    pub fn is_drag(&self) -> bool {
        let dx = self.end.x - self.start.x;
        let dy = self.end.y - self.start.y;
        dx.hypot(dy) >= self.drag_threshold
    }

    fn validate(&self) -> Result<()> {
        let point_values = [self.start.x, self.start.y, self.end.x, self.end.y];
        if point_values.iter().any(|value| !value.is_finite()) {
            return Err(Error::InvalidRequest(
                "selection gesture coordinates must be finite".to_owned(),
            ));
        }
        validate_region(self.desktop_bounds, "desktop bounds")?;
        if !self.drag_threshold.is_finite() || self.drag_threshold <= 0.0 {
            return Err(Error::InvalidRequest(format!(
                "drag threshold {} must be a positive finite distance",
                self.drag_threshold
            )));
        }
        if self.active_display.0.is_empty() {
            return Err(Error::InvalidRequest(
                "active display identifier cannot be empty".to_owned(),
            ));
        }
        if self
            .hovered_window
            .as_ref()
            .is_some_and(|window| window.0.is_empty())
        {
            return Err(Error::InvalidRequest(
                "hovered window identifier cannot be empty".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Resolves a pointer gesture to a real target.
///
/// An all-in-one click uses only a genuinely enumerated `hovered_window`; when
/// none exists it chooses the active display. It never invents a window from a
/// point. A real drag always resolves a bounded region.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] for malformed geometry, a click in
/// [`SelectionMode::Region`], an empty drag, or impossible constraints.
pub fn resolve_selection(
    mode: SelectionMode,
    gesture: &SelectionGesture,
    constraints: SelectionConstraints,
) -> Result<CaptureTarget> {
    gesture.validate()?;
    let constraints = constraints.validate()?;
    if !gesture.is_drag() {
        return match mode {
            SelectionMode::AllInOne => Ok(gesture.hovered_window.clone().map_or_else(
                || CaptureTarget::Display(gesture.active_display.clone()),
                CaptureTarget::Window,
            )),
            SelectionMode::Region => Err(Error::InvalidRequest(
                "a region selection needs a drag with non-zero area".to_owned(),
            )),
        };
    }

    let region = constrained_region(gesture, constraints)?;
    Ok(CaptureTarget::Region(region))
}

fn constrained_region(
    gesture: &SelectionGesture,
    constraints: SelectionConstraints,
) -> Result<LogicalRect> {
    let bounds = gesture.desktop_bounds;
    let start = clamp_point(gesture.start, bounds);
    let end = clamp_point(gesture.end, bounds);
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let x_sign = if gesture.end.x < gesture.start.x {
        -1.0
    } else {
        1.0
    };
    let y_sign = if gesture.end.y < gesture.start.y {
        -1.0
    } else {
        1.0
    };

    let (mut width, mut height) = if let Some(size) = constraints.exact_size {
        if size.width > bounds.size.width || size.height > bounds.size.height {
            return Err(Error::InvalidRequest(format!(
                "exact selection size {}x{} does not fit desktop bounds {}x{}",
                size.width, size.height, bounds.size.width, bounds.size.height
            )));
        }
        (size.width, size.height)
    } else {
        (dx.abs(), dy.abs())
    };

    if constraints.exact_size.is_none()
        && let Some(ratio) = constraints.aspect_ratio
    {
        let ratio = ratio.value();
        if width == 0.0 && height > 0.0 {
            width = height * ratio;
        } else if height == 0.0 || width / height > ratio {
            height = width / ratio;
        } else {
            width = height * ratio;
        }

        let fit = (bounds.size.width / width)
            .min(bounds.size.height / height)
            .min(1.0);
        width = (width * fit).min(bounds.size.width);
        height = (height * fit).min(bounds.size.height);
    }

    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(Error::InvalidRequest(
            "a dragged recording region must have non-zero area".to_owned(),
        ));
    }

    let raw_x = if x_sign < 0.0 {
        start.x - width
    } else {
        start.x
    };
    let raw_y = if y_sign < 0.0 {
        start.y - height
    } else {
        start.y
    };
    let max_x = (bounds.origin.x + bounds.size.width - width).max(bounds.origin.x);
    let max_y = (bounds.origin.y + bounds.size.height - height).max(bounds.origin.y);
    let origin = LogicalPoint::new(
        raw_x.clamp(bounds.origin.x, max_x),
        raw_y.clamp(bounds.origin.y, max_y),
    );
    let region = LogicalRect::new(origin, LogicalSize::new(width, height));
    validate_region(region, "resolved selection")?;
    Ok(region)
}

fn clamp_point(point: LogicalPoint, bounds: LogicalRect) -> LogicalPoint {
    LogicalPoint::new(
        point
            .x
            .clamp(bounds.origin.x, bounds.origin.x + bounds.size.width),
        point
            .y
            .clamp(bounds.origin.y, bounds.origin.y + bounds.size.height),
    )
}

fn validate_region(region: LogicalRect, name: &str) -> Result<()> {
    let values = [
        region.origin.x,
        region.origin.y,
        region.size.width,
        region.size.height,
    ];
    if values.iter().any(|value| !value.is_finite()) || region.is_empty() {
        return Err(Error::InvalidRequest(format!(
            "{name} must be finite and have non-zero area"
        )));
    }
    Ok(())
}

/// Memory for the most recent usable region selection.
///
/// Window and display targets do not overwrite it: only a concrete non-empty
/// region can be safely restored after windows move or displays are unplugged.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LastSelectionMemory {
    region: Option<LogicalRect>,
}

impl LastSelectionMemory {
    /// Creates empty selection memory.
    #[must_use]
    pub const fn new() -> Self {
        Self { region: None }
    }

    /// Remembers a usable region target.
    ///
    /// Returns `Ok(false)` for window/display targets without replacing an
    /// existing region.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for an empty or non-finite region.
    pub fn remember(&mut self, target: &CaptureTarget) -> Result<bool> {
        let CaptureTarget::Region(region) = target else {
            return Ok(false);
        };
        validate_region(*region, "last selection")?;
        self.region = Some(*region);
        Ok(true)
    }

    /// Restores the remembered region as a capture target.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] with a clear message when no usable
    /// region has been remembered.
    pub fn recall(&self) -> Result<CaptureTarget> {
        self.region.map(CaptureTarget::Region).ok_or_else(|| {
            Error::InvalidRequest(
                "no previous non-empty recording region has been remembered".to_owned(),
            )
        })
    }

    /// Removes remembered selection state.
    pub fn clear(&mut self) {
        self.region = None;
    }

    /// The remembered region, if one exists.
    #[must_use]
    pub const fn region(&self) -> Option<LogicalRect> {
        self.region
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> LogicalRect {
        LogicalRect::new(
            LogicalPoint::new(-100.0, 0.0),
            LogicalSize::new(500.0, 300.0),
        )
    }

    fn gesture(start: (f64, f64), end: (f64, f64)) -> SelectionGesture {
        SelectionGesture::new(
            LogicalPoint::new(start.0, start.1),
            LogicalPoint::new(end.0, end.1),
            None,
            DisplayId("active".to_owned()),
            bounds(),
        )
    }

    #[test]
    fn all_in_one_click_chooses_only_a_real_hovered_window() {
        let mut input = gesture((20.0, 20.0), (21.0, 21.0));
        input.hovered_window = Some(WindowId("window-7".to_owned()));
        assert_eq!(
            resolve_selection(SelectionMode::AllInOne, &input, SelectionConstraints::NONE).unwrap(),
            CaptureTarget::Window(WindowId("window-7".to_owned()))
        );
    }

    #[test]
    fn all_in_one_click_without_a_window_uses_the_active_display() {
        let input = gesture((20.0, 20.0), (21.0, 21.0));
        assert_eq!(
            resolve_selection(SelectionMode::AllInOne, &input, SelectionConstraints::NONE).unwrap(),
            CaptureTarget::Display(DisplayId("active".to_owned()))
        );
    }

    #[test]
    fn all_in_one_real_drag_is_always_a_region() {
        let mut input = gesture((20.0, 25.0), (120.0, 85.0));
        input.hovered_window = Some(WindowId("under-drag".to_owned()));
        let CaptureTarget::Region(region) =
            resolve_selection(SelectionMode::AllInOne, &input, SelectionConstraints::NONE).unwrap()
        else {
            panic!("a real drag must not become a window");
        };
        assert_eq!(region.origin, LogicalPoint::new(20.0, 25.0));
        assert_eq!(region.size, LogicalSize::new(100.0, 60.0));
    }

    #[test]
    fn region_mode_rejects_a_click() {
        let error = resolve_selection(
            SelectionMode::Region,
            &gesture((10.0, 10.0), (11.0, 11.0)),
            SelectionConstraints::NONE,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("needs a drag"), "{error}");
    }

    #[test]
    fn exact_size_translates_at_edges_without_changing_dimensions() {
        let target = resolve_selection(
            SelectionMode::Region,
            &gesture((390.0, 290.0), (400.0, 300.0)),
            SelectionConstraints::exact(160.0, 90.0),
        )
        .unwrap();
        let CaptureTarget::Region(region) = target else {
            panic!("expected region");
        };
        assert_eq!(region.size, LogicalSize::new(160.0, 90.0));
        assert_eq!(region.origin, LogicalPoint::new(240.0, 210.0));
    }

    #[test]
    fn exact_size_respects_drag_direction() {
        let target = resolve_selection(
            SelectionMode::Region,
            &gesture((200.0, 180.0), (150.0, 150.0)),
            SelectionConstraints::exact(100.0, 50.0),
        )
        .unwrap();
        let CaptureTarget::Region(region) = target else {
            panic!("expected region");
        };
        assert_eq!(region.origin, LogicalPoint::new(100.0, 130.0));
        assert_eq!(region.size, LogicalSize::new(100.0, 50.0));
    }

    #[test]
    fn impossible_exact_size_is_reported_not_silently_resized() {
        let error = resolve_selection(
            SelectionMode::Region,
            &gesture((0.0, 0.0), (20.0, 20.0)),
            SelectionConstraints::exact(900.0, 600.0),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("does not fit"), "{error}");
    }

    #[test]
    fn aspect_lock_preserves_ratio_and_stays_inside_desktop() {
        let ratio = AspectRatio::new(16.0, 9.0).unwrap();
        let target = resolve_selection(
            SelectionMode::Region,
            &gesture((350.0, 280.0), (-80.0, 10.0)),
            SelectionConstraints::aspect(ratio),
        )
        .unwrap();
        let CaptureTarget::Region(region) = target else {
            panic!("expected region");
        };

        assert!((region.size.width / region.size.height - 16.0 / 9.0).abs() < 1.0e-9);
        assert!(region.origin.x >= -100.0);
        assert!(region.origin.y >= 0.0);
        assert!(region.origin.x + region.size.width <= 400.0 + 1.0e-9);
        assert!(region.origin.y + region.size.height <= 300.0 + 1.0e-9);
    }

    #[test]
    fn aspect_fit_at_a_desktop_edge_cannot_invert_clamp_bounds() {
        let gesture = SelectionGesture::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalPoint::new(2.0, 295.0),
            None,
            DisplayId("active".to_owned()),
            LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(320.0, 300.0)),
        );
        let target = resolve_selection(
            SelectionMode::Region,
            &gesture,
            SelectionConstraints::aspect(AspectRatio::new(16.0, 9.0).unwrap()),
        )
        .unwrap();
        let CaptureTarget::Region(region) = target else {
            panic!("expected region");
        };

        assert_eq!(region.origin.x, 0.0);
        assert!(region.size.width <= 320.0);
    }

    #[test]
    fn exact_size_and_aspect_must_agree() {
        let error = SelectionConstraints {
            exact_size: Some(LogicalSize::new(100.0, 100.0)),
            aspect_ratio: Some(AspectRatio::new(16.0, 9.0).unwrap()),
        }
        .validate()
        .unwrap_err()
        .to_string();
        assert!(error.contains("conflicts"), "{error}");
    }

    #[test]
    fn invalid_coordinates_are_rejected() {
        let input = gesture((f64::NAN, 0.0), (10.0, 10.0));
        assert!(
            resolve_selection(SelectionMode::AllInOne, &input, SelectionConstraints::NONE).is_err()
        );
    }

    #[test]
    fn memory_only_accepts_usable_regions_and_does_not_forget_on_other_targets() {
        let region = LogicalRect::new(LogicalPoint::new(5.0, 7.0), LogicalSize::new(80.0, 45.0));
        let mut memory = LastSelectionMemory::new();
        assert!(memory.recall().is_err());
        assert!(memory.remember(&CaptureTarget::Region(region)).unwrap());
        assert!(
            !memory
                .remember(&CaptureTarget::Display(DisplayId("other".to_owned())))
                .unwrap()
        );
        assert_eq!(memory.recall().unwrap(), CaptureTarget::Region(region));

        let empty = LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(0.0, 20.0));
        assert!(memory.remember(&CaptureTarget::Region(empty)).is_err());
        assert_eq!(memory.recall().unwrap(), CaptureTarget::Region(region));

        memory.clear();
        let error = memory.recall().unwrap_err().to_string();
        assert!(error.contains("no previous"), "{error}");
    }
}
