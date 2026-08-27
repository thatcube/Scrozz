//! Deterministic desktop layouts for the window picker.
//!
//! These values deliberately model the awkward cases the picker must handle:
//! z-order overlap, Scrozz's own fullscreen overlay, minimised windows, tiled
//! half-open edges, and mixed display scale factors. They contain no host state,
//! so the same picker behavior is exercised on every CI platform.

use scrozz_core::{
    Display, DisplayId, LogicalPoint, LogicalRect, LogicalSize, ScaleFactor, Window, WindowId,
};

use super::WindowPicker;

/// A complete picker snapshot.
#[derive(Debug, Clone)]
pub struct PickerFixture {
    /// Stable scenario name for diagnostics.
    pub name: &'static str,
    /// Front-most first, matching `TargetEnumerator::windows`.
    pub windows: Vec<Window>,
    /// Connected displays in global logical desktop coordinates.
    pub displays: Vec<Display>,
    /// Scrozz-owned windows that must not be selectable.
    pub excluded: Vec<WindowId>,
}

impl PickerFixture {
    /// Builds the picker represented by this snapshot.
    #[must_use]
    pub fn into_picker(self) -> WindowPicker {
        WindowPicker::new(self.windows, self.displays).excluding(self.excluded)
    }
}

/// One 1440-by-900 logical primary display.
#[must_use]
pub fn single_display() -> Vec<Display> {
    vec![display(
        "main",
        "Built-in Display",
        0.0,
        0.0,
        1440.0,
        900.0,
        2.0,
        true,
    )]
}

/// Two ordinary windows whose visible frames overlap.
#[must_use]
pub fn overlapping() -> PickerFixture {
    PickerFixture {
        name: "overlapping",
        windows: vec![
            window(
                "front",
                "GitHub",
                "Safari",
                "com.apple.Safari",
                200.0,
                150.0,
                600.0,
                400.0,
                "main",
                true,
            ),
            window(
                "back",
                "Projects",
                "Finder",
                "com.apple.finder",
                100.0,
                100.0,
                800.0,
                600.0,
                "main",
                true,
            ),
        ],
        displays: single_display(),
        excluded: Vec::new(),
    }
}

/// The overlap fixture with Scrozz's fullscreen picker overlay in front.
#[must_use]
pub fn with_our_overlay() -> PickerFixture {
    let mut fixture = overlapping();
    let overlay_id = WindowId("scrozz-overlay".to_owned());
    fixture.name = "our-overlay";
    fixture.windows.insert(
        0,
        window(
            &overlay_id.0,
            "Window picker",
            "Scrozz",
            "com.thatcube.scrozz",
            0.0,
            0.0,
            1440.0,
            900.0,
            "main",
            true,
        ),
    );
    fixture.excluded.push(overlay_id);
    fixture
}

/// One selectable window, one minimised window, and Scrozz's own overlay.
#[must_use]
pub fn with_minimised() -> PickerFixture {
    PickerFixture {
        name: "minimised",
        windows: vec![
            window(
                "scrozz-overlay",
                "Window picker",
                "Scrozz",
                "com.thatcube.scrozz",
                0.0,
                0.0,
                1440.0,
                900.0,
                "main",
                true,
            ),
            window(
                "minimised",
                "Downloads",
                "Finder",
                "com.apple.finder",
                800.0,
                600.0,
                200.0,
                200.0,
                "main",
                false,
            ),
            window(
                "visible",
                "GitHub",
                "Safari",
                "com.apple.Safari",
                100.0,
                100.0,
                600.0,
                400.0,
                "main",
                true,
            ),
        ],
        displays: single_display(),
        excluded: vec![WindowId("scrozz-overlay".to_owned())],
    }
}

/// Two windows tiled against the same vertical edge.
#[must_use]
pub fn tiled() -> PickerFixture {
    PickerFixture {
        name: "tiled",
        windows: vec![
            window(
                "left",
                "Left",
                "Terminal",
                "com.apple.Terminal",
                0.0,
                0.0,
                500.0,
                800.0,
                "main",
                true,
            ),
            window(
                "right",
                "Right",
                "Editor",
                "com.microsoft.VSCode",
                500.0,
                0.0,
                500.0,
                800.0,
                "main",
                true,
            ),
        ],
        displays: single_display(),
        excluded: Vec::new(),
    }
}

/// A 2x primary display beside a 1x external display.
#[must_use]
pub fn mixed_dpi() -> PickerFixture {
    PickerFixture {
        name: "mixed-dpi",
        windows: vec![
            window(
                "retina-window",
                "Retina",
                "Preview",
                "com.apple.Preview",
                200.0,
                200.0,
                600.0,
                400.0,
                "retina",
                true,
            ),
            window(
                "external-window",
                "External",
                "Firefox",
                "org.mozilla.firefox",
                2000.0,
                250.0,
                500.0,
                300.0,
                "external",
                true,
            ),
            // 300 points overlap the Retina display and 500 the external.
            window(
                "straddling-window",
                "Straddling",
                "Notes",
                "com.apple.Notes",
                1620.0,
                200.0,
                800.0,
                400.0,
                "external",
                true,
            ),
        ],
        displays: vec![
            display(
                "retina",
                "Built-in Retina Display",
                0.0,
                0.0,
                1920.0,
                1080.0,
                2.0,
                true,
            ),
            display(
                "external",
                "External Display",
                1920.0,
                0.0,
                1920.0,
                1080.0,
                1.0,
                false,
            ),
        ],
        excluded: Vec::new(),
    }
}

/// One visible window and one malformed zero-area window.
#[must_use]
pub fn with_zero_area() -> PickerFixture {
    PickerFixture {
        name: "zero-area",
        windows: vec![
            window(
                "zero",
                "Zero",
                "Broken client",
                "invalid",
                50.0,
                50.0,
                0.0,
                0.0,
                "main",
                true,
            ),
            window(
                "visible",
                "Visible",
                "Safari",
                "com.apple.Safari",
                200.0,
                200.0,
                600.0,
                400.0,
                "main",
                true,
            ),
        ],
        displays: single_display(),
        excluded: Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn display(
    id: &str,
    name: &str,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
    is_primary: bool,
) -> Display {
    let bounds = rect(x, y, width, height);
    Display {
        id: DisplayId(id.to_owned()),
        name: name.to_owned(),
        bounds,
        work_area: bounds,
        scale: ScaleFactor::new(scale),
        is_primary,
    }
}

#[allow(clippy::too_many_arguments)]
fn window(
    id: &str,
    title: &str,
    application: &str,
    application_id: &str,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    display: &str,
    is_visible: bool,
) -> Window {
    Window {
        id: WindowId(id.to_owned()),
        title: Some(title.to_owned()),
        application: Some(application.to_owned()),
        application_id: Some(application_id.to_owned()),
        bounds: rect(x, y, width, height),
        display: DisplayId(display.to_owned()),
        is_visible,
    }
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> LogicalRect {
    LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(width, height))
}
