//! The macOS still-capture backend, built on ScreenCaptureKit.
//!
//! # Shape of the implementation
//!
//! Enumeration and capture come from different places on purpose. Displays are
//! enumerated through CoreGraphics ([`display`]), which works from any thread,
//! needs no permission, and reports the true backing-scale factor. Windows and
//! captures go through ScreenCaptureKit, which does need permission — and asks
//! for it, per decision D15, at the moment of first use rather than up front.
//! Display filters exclude every window owned by the current process, allowing
//! Scrozz's capture cards to stay visually stable without appearing in output.
//!
//! # macOS version floor
//!
//! `SCScreenshotManager` is macOS 14 and later, and is used because a still
//! capture through it is a single call. The pre-14 alternative is spelled out
//! in [`sck::capture_image_with_cancellation`]; on an older system this backend reports
//! [`scrozz_core::Error::Unsupported`] rather than misbehaving.

mod appkit;
mod display;
mod error;
mod image;
mod picker;
mod sck;
pub(crate) mod scroll;
mod window;

use objc2::rc::Retained;
use objc2_foundation::NSArray;
use objc2_screen_capture_kit::{
    SCCaptureResolutionType, SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent,
    SCStreamConfiguration, SCWindow,
};
use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, CursorMode, Display, Error,
    LogicalRect, Provenance, Result, ScaleFactor, TargetEnumerator, Window,
};

use crate::{CaptureCancellation, FrameSession};

/// ScreenCaptureKit-backed still capture.
#[derive(Debug, Default)]
pub struct ScreenCaptureKitBackend {
    cancellation: Option<CaptureCancellation>,
}

pub use picker::{
    AppleContentPicker, ApplePickerAvailability, ApplePickerEvent, ApplePickerMode, PickerCapture,
};

/// Whether the running macOS has Scrozz's public still-capture API.
#[must_use]
pub fn still_capture_available() -> bool {
    sck::supports_still_capture()
}

impl ScreenCaptureKitBackend {
    /// Creates the backend.
    ///
    /// Deliberately does no work and asks for no permission: constructing a
    /// backend must not put a system dialog on screen. That happens on the
    /// first call that genuinely needs access.
    #[must_use]
    pub const fn new() -> Self {
        Self { cancellation: None }
    }

    pub(crate) fn with_cancellation(cancellation: &CaptureCancellation) -> Self {
        Self {
            cancellation: Some(cancellation.clone()),
        }
    }
}

impl TargetEnumerator for ScreenCaptureKitBackend {
    fn displays(&self) -> Result<Vec<Display>> {
        display::displays()
    }

    fn windows(&self) -> Result<Vec<Window>> {
        let content = sck::shareable_content()?;
        let displays = display::displays()?;
        window::windows(&content, &displays)
    }

    fn active_display(&self) -> Result<Display> {
        display::active_display()
    }
}

struct WindowFrameSession {
    filter: Retained<SCContentFilter>,
    configuration: Retained<SCStreamConfiguration>,
    scale: ScaleFactor,
    cancellation: Option<CaptureCancellation>,
}

impl FrameSession for WindowFrameSession {
    fn capture_frame(&mut self) -> Result<scrozz_core::Frame> {
        if let Some(cancellation) = &self.cancellation {
            cancellation.check()?;
        }
        let image = sck::capture_image_with_cancellation(
            &self.filter,
            &self.configuration,
            self.cancellation.as_ref(),
        )?;
        let frame = image::to_frame(&image, self.scale)?;
        if let Some(cancellation) = &self.cancellation {
            cancellation.check()?;
        }
        Ok(frame)
    }

    fn name(&self) -> &str {
        "ScreenCaptureKit window session"
    }
}

pub(crate) fn window_frame_session(
    request: CaptureRequest,
    cancellation: Option<&CaptureCancellation>,
) -> Result<Box<dyn FrameSession>> {
    let CaptureTarget::Window(id) = &request.target else {
        return Err(Error::InvalidRequest(
            "a reusable macOS window session requires one window target".to_owned(),
        ));
    };
    let content = sck::shareable_content_with_cancellation(cancellation)?;
    let target = window::find(&content, id).ok_or_else(|| {
        Error::TargetGone(format!(
            "window {} is no longer open for scrolling capture",
            id.0
        ))
    })?;
    let fallback_scale = window_scale(&target);
    let filter =
        unsafe { SCContentFilter::initWithDesktopIndependentWindow(sck::alloc_filter(), &target) };
    let (configuration, scale) = configure(&filter, &request, fallback_scale, None)?;
    Ok(Box::new(WindowFrameSession {
        filter,
        configuration,
        scale,
        cancellation: cancellation.cloned(),
    }))
}

impl CaptureBackend for ScreenCaptureKitBackend {
    fn excludes_current_process(&self, target: &CaptureTarget) -> bool {
        matches!(target, CaptureTarget::Display(_))
    }

    fn capture(&self, request: &CaptureRequest) -> Result<Capture> {
        if let Some(cancellation) = &self.cancellation {
            cancellation.check()?;
        }
        let (frame, provenance) = match &request.target {
            CaptureTarget::Display(id) => capture_display(id, request, self.cancellation.as_ref())?,
            CaptureTarget::Window(id) => capture_window(id, request, self.cancellation.as_ref())?,
            CaptureTarget::Region(rect) => {
                capture_region(*rect, request, self.cancellation.as_ref())?
            }
            CaptureTarget::AllDisplays => {
                capture_all_displays(request, self.cancellation.as_ref())?
            }
        };
        if let Some(cancellation) = &self.cancellation {
            cancellation.check()?;
        }

        Ok(Capture {
            frame,
            provenance,
            target: request.target.clone(),
        })
    }

    fn name(&self) -> &str {
        "ScreenCaptureKit"
    }
}

fn capture_display(
    id: &scrozz_core::DisplayId,
    request: &CaptureRequest,
    cancellation: Option<&CaptureCancellation>,
) -> Result<(scrozz_core::Frame, Provenance)> {
    let content = sck::shareable_content_with_cancellation(cancellation)?;
    let target = find_display(&content, id)?;

    // SAFETY: `displayID` is a plain property read.
    let scale = display::scale_factor(unsafe { target.displayID() });

    let filter = display_filter_excluding_current_process(&content, &target);

    let (config, scale) = configure(&filter, request, scale, None)?;
    let image = sck::capture_image_with_cancellation(&filter, &config, cancellation)?;
    Ok((image::to_frame(&image, scale)?, Provenance::Display))
}

/// Captures one window, and nothing else.
///
/// Decision D9 makes this the sacred path: whatever ScreenCaptureKit returns is
/// the window's true shape, including its real shadow and its real corner
/// radius. Nothing here pads, rounds, recolours or composites — the request's
/// `include_window_shadow` is passed to the compositor, which knows how to
/// render the shadow the window actually has, and the resulting image's own
/// dimensions are taken as final.
///
/// # What the shadow flag actually does
///
/// Measured on macOS 15 with `SCScreenshotManager`: a desktop-independent
/// window filter reports a `contentRect` exactly equal to the window's frame,
/// and toggling `ignoreShadowsSingleWindow` changes neither that rectangle nor
/// the returned image's dimensions. The property is meaningful for `SCStream`
/// capture, where the window is composited onto a display-sized surface and the
/// shadow has somewhere to fall.
///
/// The flag is still passed through rather than dropped, because that is the
/// request the caller made and the OS is entitled to honour it on other
/// versions. What this function must never do is *manufacture* the difference
/// by padding the image out and drawing a shadow itself; that is precisely the
/// compositing D9 forbids, and it is why the returned image's own size is
/// treated as authoritative rather than checked against `contentRect`.
fn capture_window(
    id: &scrozz_core::WindowId,
    request: &CaptureRequest,
    cancellation: Option<&CaptureCancellation>,
) -> Result<(scrozz_core::Frame, Provenance)> {
    let content = sck::shareable_content_with_cancellation(cancellation)?;
    let target = window::find(&content, id).ok_or_else(|| {
        Error::TargetGone(format!(
            "window {} is no longer open; it may have been closed since the list was taken",
            id.0
        ))
    })?;

    // The scale must come from the display the window is actually on. Using the
    // primary display's scale would capture a window on a 1× external monitor
    // at half size, or a 2× window at double.
    let scale = window_scale(&target);

    // SAFETY: the desktop-independent-window initialiser, which captures the
    // window alone regardless of what overlaps it.
    let filter =
        unsafe { SCContentFilter::initWithDesktopIndependentWindow(sck::alloc_filter(), &target) };

    let (config, scale) = configure(&filter, request, scale, None)?;
    let image = sck::capture_image_with_cancellation(&filter, &config, cancellation)?;
    Ok((image::to_frame(&image, scale)?, Provenance::Window))
}

/// Captures a rectangle of the global desktop.
///
/// Prefers `captureImageInRect:`, which resolves the rectangle against every
/// display itself and so handles a selection dragged across two monitors. Where
/// that is unavailable, falls back to cropping within the one display that
/// contains the rectangle — a narrower capability, reported honestly rather
/// than approximated.
fn capture_region(
    rect: LogicalRect,
    request: &CaptureRequest,
    cancellation: Option<&CaptureCancellation>,
) -> Result<(scrozz_core::Frame, Provenance)> {
    if rect.is_empty() {
        return Err(Error::InvalidRequest(
            "a capture region must have a non-zero width and height".to_owned(),
        ));
    }

    let displays = display::displays()?;
    let scale = scale_for_rect(rect, &displays);

    if sck::supports_capture_in_rect() {
        let image =
            sck::capture_image_in_rect_with_cancellation(display::to_cg_rect(rect), cancellation)?;
        return Ok((image::to_frame(&image, scale)?, Provenance::Region));
    }

    let home = displays
        .iter()
        .find(|display| overlaps(display.bounds, rect))
        .ok_or_else(|| {
            Error::InvalidRequest("the requested region is not on any display".to_owned())
        })?;

    let content = sck::shareable_content_with_cancellation(cancellation)?;
    let target = find_display(&content, &home.id)?;

    let filter = display_filter_excluding_current_process(&content, &target);

    // `sourceRect` is in the display's own points, not global desktop points.
    let local = LogicalRect::new(
        scrozz_core::LogicalPoint::new(
            rect.origin.x - home.bounds.origin.x,
            rect.origin.y - home.bounds.origin.y,
        ),
        rect.size,
    );

    let (config, scale) = configure(&filter, request, home.scale, Some(local))?;
    let image = sck::capture_image_with_cancellation(&filter, &config, cancellation)?;
    Ok((image::to_frame(&image, scale)?, Provenance::Region))
}

/// Captures every display as one image.
///
/// This is `captureImageInRect:` over the union of all display bounds, which
/// lets ScreenCaptureKit do the compositing. Doing it here instead would mean
/// resampling each display into a common scale and guessing at the gaps between
/// non-adjacent monitors — a worse image, produced more slowly. A single
/// display short-circuits to the ordinary display path.
fn capture_all_displays(
    request: &CaptureRequest,
    cancellation: Option<&CaptureCancellation>,
) -> Result<(scrozz_core::Frame, Provenance)> {
    let displays = display::displays()?;
    match displays.as_slice() {
        [] => Err(Error::Unsupported {
            what: "capturing all displays".to_owned(),
            why: "no displays are attached".to_owned(),
        }),
        [only] => {
            let (frame, _) = capture_display(&only.id, request, cancellation)?;
            Ok((frame, Provenance::AllDisplays))
        }
        many => {
            if !sck::supports_capture_in_rect() {
                return Err(Error::Unsupported {
                    what: "capturing all displays as one image".to_owned(),
                    why: "this macOS lacks the API that composites across displays; \
                          capture each display separately"
                        .to_owned(),
                });
            }

            let union = union_of(many);
            let scale = scale_for_rect(union, many);
            let image = sck::capture_image_in_rect_with_cancellation(
                display::to_cg_rect(union),
                cancellation,
            )?;
            Ok((image::to_frame(&image, scale)?, Provenance::AllDisplays))
        }
    }
}

/// Builds the stream configuration for a capture.
///
/// The pixel dimensions are computed from the filter's own content rect times
/// its point-to-pixel scale, so the capture comes back at the display's native
/// resolution. This is not optional: `SCStreamConfiguration` defaults to
/// 1920×1080, and a configuration left alone returns a 1920×1080 image for a
/// 3456×2234 display — measured, not assumed. That is exactly the blurry
/// screenshot this must avoid.
fn configure(
    filter: &SCContentFilter,
    request: &CaptureRequest,
    fallback_scale: ScaleFactor,
    source_rect: Option<LogicalRect>,
) -> Result<(Retained<SCStreamConfiguration>, ScaleFactor)> {
    let config = unsafe { SCStreamConfiguration::new() };
    // SAFETY: immutable property read from the live filter.
    let pixel_scale = f64::from(unsafe { filter.pointPixelScale() });
    let scale = if pixel_scale.is_finite() && pixel_scale > 0.0 {
        display::scale_from_ratio(pixel_scale)
    } else {
        fallback_scale
    };

    // SAFETY: all plain property reads and writes on a fresh configuration.
    unsafe {
        // The filter knows its own pixel scale on macOS 14+; fall back to the
        // display's when it reports something implausible.
        let content = filter.contentRect();
        let region = source_rect.unwrap_or_else(|| display::from_cg_rect(content));
        let pixels = region.to_physical(scale);

        let (width, height) = (pixels.pixel_width(), pixels.pixel_height());
        if width == 0 || height == 0 {
            return Err(Error::InvalidRequest(
                "the requested capture has no area".to_owned(),
            ));
        }

        config.setWidth(width as usize);
        config.setHeight(height as usize);

        // Native resolution, not a downscale to fit the configured size.
        config.setCaptureResolution(SCCaptureResolutionType::Best);
        config.setScalesToFit(false);
        config.setPreservesAspectRatio(true);

        if source_rect.is_some() {
            config.setSourceRect(display::to_cg_rect(region));
        }

        config.setShowsCursor(request.cursor == CursorMode::Visible);

        // Decision D9 again: the shadow is the window's own, drawn by the
        // compositor. This only chooses whether to ask for it.
        config.setIgnoreShadowsSingleWindow(!request.include_window_shadow);
        config.setIgnoreShadowsDisplay(false);
        config.setIgnoreGlobalClipSingleWindow(true);
        config.setShouldBeOpaque(false);

        // `colorSpaceName` is deliberately left untouched. Setting it would
        // convert the capture into that space; leaving it lets ScreenCaptureKit
        // deliver the display's own — Display P3 on most modern Macs — which
        // the frame then reports honestly.
    }

    Ok((config, scale))
}

/// Builds a display filter that leaves Scrozz visible to the user but absent
/// from the resulting pixels.
///
/// ScreenCaptureKit restores whatever is behind excluded application windows;
/// it does not paint a blank rectangle. This is the native mechanism that lets
/// the capture stack remain visually stable during a whole-display capture.
fn display_filter_excluding_current_process(
    content: &SCShareableContent,
    display: &SCDisplay,
) -> Retained<SCContentFilter> {
    let current_pid = i32::try_from(std::process::id()).ok();
    // SAFETY: these are immutable property reads from one shareable-content
    // snapshot.
    let excluded: Vec<Retained<SCRunningApplication>> = unsafe {
        content
            .applications()
            .iter()
            .filter(|application| Some(application.processID()) == current_pid)
            .collect()
    };
    let excluded = NSArray::from_retained_slice(&excluded);

    // SAFETY: designated whole-display filter initialiser. With no matching
    // application this is equivalent to an empty exclusion list.
    unsafe {
        SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
            sck::alloc_filter(),
            display,
            &excluded,
            &NSArray::new(),
        )
    }
}

fn find_display(
    content: &SCShareableContent,
    id: &scrozz_core::DisplayId,
) -> Result<Retained<SCDisplay>> {
    let wanted = display::parse_display_id(id)
        .ok_or_else(|| Error::InvalidRequest(format!("{:?} is not a macOS display id", id.0)))?;

    // SAFETY: reading the snapshot's display list and each display's ID.
    unsafe {
        content
            .displays()
            .iter()
            .find(|display| display.displayID() == wanted)
    }
    .ok_or_else(|| {
        Error::TargetGone(format!(
            "display {wanted} is no longer attached or is not shareable"
        ))
    })
}

fn window_scale(window: &SCWindow) -> ScaleFactor {
    // SAFETY: a plain property read.
    let frame = display::from_cg_rect(unsafe { window.frame() });
    let centre = (
        frame.origin.x + frame.size.width / 2.0,
        frame.origin.y + frame.size.height / 2.0,
    );

    let displays = display::displays().unwrap_or_default();
    displays
        .iter()
        .find(|display| display::contains(display.bounds, centre))
        .or_else(|| displays.iter().find(|display| display.is_primary))
        .map_or(ScaleFactor::IDENTITY, |display| display.scale)
}

/// The scale to record for a rectangle that may span displays.
///
/// The largest scale of any display it touches, because that is the resolution
/// ScreenCaptureKit renders the composite at: taking the smallest instead would
/// claim a Retina capture was 1× and halve its logical size.
fn scale_for_rect(rect: LogicalRect, displays: &[Display]) -> ScaleFactor {
    displays
        .iter()
        .filter(|display| overlaps(display.bounds, rect))
        .map(|display| display.scale.get())
        .fold(None, |best: Option<f64>, scale| {
            Some(best.map_or(scale, |best| best.max(scale)))
        })
        .map_or(ScaleFactor::IDENTITY, display::scale_from_ratio)
}

fn union_of(displays: &[Display]) -> LogicalRect {
    let mut left = f64::MAX;
    let mut top = f64::MAX;
    let mut right = f64::MIN;
    let mut bottom = f64::MIN;

    for display in displays {
        left = left.min(display.bounds.origin.x);
        top = top.min(display.bounds.origin.y);
        right = right.max(display.bounds.origin.x + display.bounds.size.width);
        bottom = bottom.max(display.bounds.origin.y + display.bounds.size.height);
    }

    LogicalRect::new(
        scrozz_core::LogicalPoint::new(left, top),
        scrozz_core::LogicalSize::new(right - left, bottom - top),
    )
}

fn overlaps(a: LogicalRect, b: LogicalRect) -> bool {
    a.origin.x < b.origin.x + b.size.width
        && b.origin.x < a.origin.x + a.size.width
        && a.origin.y < b.origin.y + b.size.height
        && b.origin.y < a.origin.y + a.size.height
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{DisplayId, LogicalPoint, LogicalSize};

    fn display_at(id: &str, x: f64, width: f64, scale: f64) -> Display {
        let bounds = LogicalRect::new(LogicalPoint::new(x, 0.0), LogicalSize::new(width, 1000.0));
        Display {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds,
            work_area: bounds,
            scale: ScaleFactor::new(scale),
            is_primary: x == 0.0,
        }
    }

    fn rect(x: f64, width: f64) -> LogicalRect {
        LogicalRect::new(LogicalPoint::new(x, 0.0), LogicalSize::new(width, 100.0))
    }

    #[test]
    fn the_union_spans_every_display_including_negative_placements() {
        let displays = [
            display_at("left", -1920.0, 1920.0, 1.0),
            display_at("main", 0.0, 1512.0, 2.0),
        ];
        let union = union_of(&displays);

        assert_eq!(union.origin.x, -1920.0);
        assert_eq!(union.size.width, 1920.0 + 1512.0);
        assert_eq!(union.size.height, 1000.0);
    }

    #[test]
    fn a_rect_spanning_displays_takes_the_higher_scale() {
        let displays = [
            display_at("main", 0.0, 1000.0, 2.0),
            display_at("external", 1000.0, 1000.0, 1.0),
        ];
        // Straddles both.
        assert_eq!(scale_for_rect(rect(900.0, 200.0), &displays).get(), 2.0);
        // Wholly on the 1× display.
        assert_eq!(scale_for_rect(rect(1100.0, 200.0), &displays).get(), 1.0);
    }

    #[test]
    fn a_rect_on_no_display_falls_back_to_identity_rather_than_panicking() {
        let displays = [display_at("main", 0.0, 1000.0, 2.0)];
        assert_eq!(scale_for_rect(rect(-9000.0, 10.0), &displays).get(), 1.0);
    }

    #[test]
    fn touching_edges_do_not_count_as_overlapping() {
        assert!(!overlaps(rect(0.0, 100.0), rect(100.0, 100.0)));
        assert!(overlaps(rect(0.0, 100.0), rect(99.0, 100.0)));
    }

    /// The property decision D9 turns on, asserted where the backend sets it.
    #[test]
    fn window_captures_declare_themselves_uncompositable() {
        assert!(Provenance::Window.forbids_compositing());
        assert!(!Provenance::Display.forbids_compositing());
        assert!(!Provenance::Region.forbids_compositing());
    }

    #[test]
    fn the_backend_names_itself_for_bug_reports() {
        assert_eq!(ScreenCaptureKitBackend::new().name(), "ScreenCaptureKit");
    }

    #[test]
    fn only_filtered_display_captures_promise_current_process_exclusion() {
        let backend = ScreenCaptureKitBackend::new();
        assert!(backend.excludes_current_process(&CaptureTarget::Display(DisplayId("1".into()))));
        assert!(!backend.excludes_current_process(&CaptureTarget::Region(rect(0.0, 100.0))));
        assert!(!backend.excludes_current_process(&CaptureTarget::AllDisplays));
        assert!(
            !backend.excludes_current_process(&CaptureTarget::Window(scrozz_core::WindowId(
                "1".into()
            )))
        );
    }
}
