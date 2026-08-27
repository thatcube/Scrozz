//! Resolve a public capture target into one ScreenCaptureKit content filter.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::NSObjectProtocol;
use objc2::sel;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCRunningApplication, SCShareableContent, SCWindow,
};
use scrozz_core::{CaptureTarget, Error, LogicalPoint, LogicalRect, LogicalSize, Result};

use super::{error, permission};

pub(crate) struct CaptureContent {
    pub(crate) sources: Vec<CaptureSource>,
    pub(crate) native_width: u32,
    pub(crate) native_height: u32,
    pub(crate) scale: f64,
    canvas: LogicalRect,
    composite: bool,
}

pub(crate) struct CaptureSource {
    pub(crate) filter: Retained<SCContentFilter>,
    pub(crate) source_rect: Option<CGRect>,
    pub(crate) label: String,
    pub(crate) terminal_inactivity: bool,
    destination: LogicalRect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixelRect {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl CaptureContent {
    pub(crate) fn requires_composition(&self) -> bool {
        self.composite
    }

    pub(crate) fn output_rect(
        &self,
        source_index: usize,
        output_width: u32,
        output_height: u32,
    ) -> PixelRect {
        if !self.composite {
            return PixelRect {
                x: 0,
                y: 0,
                width: output_width,
                height: output_height,
            };
        }
        map_rect(
            self.sources[source_index].destination,
            self.canvas,
            output_width,
            output_height,
        )
    }
}

struct ContentDelivery {
    content: Option<Retained<SCShareableContent>>,
    failure: Option<Retained<NSError>>,
}

// SAFETY: the completion handler transfers retained immutable Objective-C
// objects once under a mutex; they are not accessed concurrently.
unsafe impl Send for ContentDelivery {}

pub(crate) fn resolve(target: &CaptureTarget) -> Result<CaptureContent> {
    let content = shareable_content()?;
    let excluded_applications = current_process_applications(&content);
    match target {
        CaptureTarget::Display(id) => {
            let display_id = id.0.parse::<u32>().map_err(|_| {
                Error::InvalidRequest(format!("{:?} is not a macOS display id", id.0))
            })?;
            let display = find_display(&content, display_id)?;
            whole_display(&display, &excluded_applications)
        }
        CaptureTarget::Window(id) => {
            let window_id = id.0.parse::<u32>().map_err(|_| {
                Error::InvalidRequest(format!("{:?} is not a macOS window id", id.0))
            })?;
            let window = find_window(&content, window_id)?;
            window_content(&content, &window)
        }
        CaptureTarget::Region(rect) => region_content(&content, *rect, &excluded_applications),
        CaptureTarget::AllDisplays => all_displays(&content, &excluded_applications),
    }
}

fn whole_display(
    display: &SCDisplay,
    excluded_applications: &NSArray<SCRunningApplication>,
) -> Result<CaptureContent> {
    // SAFETY: this is ScreenCaptureKit's designated display filter initializer;
    // both exclusion lists remain alive for the call.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
            SCContentFilter::alloc(),
            display,
            excluded_applications,
            &NSArray::new(),
        )
    };
    let fallback_scale = display_scale(display);
    // SAFETY: immutable geometry/id reads from the shareable-content snapshot.
    let (frame, display_id) = unsafe { (display.frame(), display.displayID()) };
    let (native_width, native_height, scale) =
        filter_geometry(&filter, fallback_scale, frame.size)?;
    let canvas = logical_rect(frame);
    Ok(CaptureContent {
        sources: vec![CaptureSource {
            filter,
            source_rect: None,
            label: format!("display {display_id}"),
            terminal_inactivity: false,
            destination: canvas,
        }],
        native_width,
        native_height,
        scale,
        canvas,
        composite: false,
    })
}

fn window_content(content: &SCShareableContent, window: &SCWindow) -> Result<CaptureContent> {
    // SAFETY: this is the designated independent-window initializer.
    let filter = unsafe {
        SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window)
    };
    // SAFETY: immutable geometry reads from the shareable-content snapshot.
    let (frame, window_id) = unsafe { (window.frame(), window.windowID()) };
    let fallback_scale = window_scale(content, frame);
    let (native_width, native_height, scale) =
        filter_geometry(&filter, fallback_scale, frame.size)?;
    let canvas = LogicalRect::new(
        LogicalPoint::new(0.0, 0.0),
        LogicalSize::new(
            f64::from(native_width) / scale,
            f64::from(native_height) / scale,
        ),
    );
    Ok(CaptureContent {
        sources: vec![CaptureSource {
            filter,
            source_rect: None,
            label: format!("window {window_id}"),
            terminal_inactivity: true,
            destination: canvas,
        }],
        native_width,
        native_height,
        scale,
        canvas,
        composite: false,
    })
}

fn region_content(
    content: &SCShareableContent,
    rect: scrozz_core::LogicalRect,
    excluded_applications: &NSArray<SCRunningApplication>,
) -> Result<CaptureContent> {
    let displays = display_snapshots(content);
    if let Some(display) = displays
        .iter()
        .find(|display| contains_rect(display.frame, rect))
    {
        return region_on_one_display(display, rect, excluded_applications);
    }
    let mut participating: Vec<_> = displays
        .into_iter()
        .filter_map(|display| {
            intersection(logical_rect(display.frame), rect).map(|visible| (display, visible))
        })
        .collect();
    if participating.is_empty() {
        return Err(Error::InvalidRequest(
            "the recording region does not intersect a shareable display".to_owned(),
        ));
    }
    sort_primary_first(&mut participating, |entry| entry.0.id);
    let scale = participating
        .iter()
        .map(|entry| entry.0.scale)
        .fold(f64::INFINITY, f64::min);
    let (native_width, native_height) =
        dimensions(rect.size.width * scale, rect.size.height * scale)?;
    let sources = participating
        .into_iter()
        .map(|(display, visible)| {
            let display_frame = logical_rect(display.frame);
            let source_rect = CGRect::new(
                CGPoint::new(
                    visible.origin.x - display_frame.origin.x,
                    visible.origin.y - display_frame.origin.y,
                ),
                CGSize::new(visible.size.width, visible.size.height),
            );
            Ok(CaptureSource {
                filter: display_filter(&display.display, excluded_applications),
                source_rect: Some(source_rect),
                label: format!("display {}", display.id),
                terminal_inactivity: false,
                destination: visible,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(CaptureContent {
        sources,
        native_width,
        native_height,
        scale,
        canvas: rect,
        composite: true,
    })
}

fn region_on_one_display(
    display: &DisplaySnapshot,
    rect: LogicalRect,
    excluded_applications: &NSArray<SCRunningApplication>,
) -> Result<CaptureContent> {
    // SAFETY: immutable geometry read.
    let display_frame = display.frame;
    let source_rect = CGRect::new(
        CGPoint::new(
            rect.origin.x - display_frame.origin.x,
            rect.origin.y - display_frame.origin.y,
        ),
        CGSize::new(rect.size.width, rect.size.height),
    );
    // SAFETY: a whole-display filter is required for region streams; sourceRect
    // performs the crop in the display's local point coordinate space.
    let filter = display_filter(&display.display, excluded_applications);
    let scale = filter_scale(&filter, display.scale);
    let (native_width, native_height) =
        dimensions(rect.size.width * scale, rect.size.height * scale)?;
    Ok(CaptureContent {
        sources: vec![CaptureSource {
            filter,
            source_rect: Some(source_rect),
            label: format!("display {}", display.id),
            terminal_inactivity: false,
            destination: rect,
        }],
        native_width,
        native_height,
        scale,
        canvas: rect,
        composite: false,
    })
}

fn all_displays(
    content: &SCShareableContent,
    excluded_applications: &NSArray<SCRunningApplication>,
) -> Result<CaptureContent> {
    let mut displays = display_snapshots(content);
    match displays.len() {
        0 => {
            return Err(Error::Unsupported {
                what: "recording all displays".to_owned(),
                why: "no shareable displays are attached".to_owned(),
            });
        }
        1 => return whole_display(&displays[0].display, excluded_applications),
        _ => {}
    }

    sort_primary_first(&mut displays, |display| display.id);
    let canvas = displays
        .iter()
        .map(|display| logical_rect(display.frame))
        .reduce(union)
        .expect("more than one display has a union");
    let scale = displays
        .iter()
        .map(|display| display.scale)
        .fold(f64::INFINITY, f64::min);
    let (native_width, native_height) =
        dimensions(canvas.size.width * scale, canvas.size.height * scale)?;
    let sources = displays
        .into_iter()
        .map(|display| CaptureSource {
            filter: display_filter(&display.display, excluded_applications),
            source_rect: None,
            label: format!("display {}", display.id),
            terminal_inactivity: false,
            destination: logical_rect(display.frame),
        })
        .collect();

    Ok(CaptureContent {
        sources,
        native_width,
        native_height,
        scale,
        canvas,
        composite: true,
    })
}

struct DisplaySnapshot {
    display: Retained<SCDisplay>,
    id: u32,
    frame: CGRect,
    scale: f64,
}

fn display_snapshots(content: &SCShareableContent) -> Vec<DisplaySnapshot> {
    // SAFETY: immutable reads from one retained shareable-content snapshot.
    unsafe {
        content
            .displays()
            .iter()
            .map(|display| DisplaySnapshot {
                id: display.displayID(),
                frame: display.frame(),
                scale: display_scale(&display),
                display,
            })
            .collect()
    }
}

fn sort_primary_first<T>(values: &mut [T], id: impl Fn(&T) -> u32) {
    let primary = CGMainDisplayID();
    values.sort_by_key(|value| {
        let value_id = id(value);
        (value_id != primary, value_id)
    });
}

fn display_filter(
    display: &SCDisplay,
    excluded_applications: &NSArray<SCRunningApplication>,
) -> Retained<SCContentFilter> {
    // SAFETY: designated initializer with a live display and retained exclusion
    // lists. Excluding this process prevents Scrozz's HUD and private cards from
    // entering display- or region-recording frames.
    unsafe {
        SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
            SCContentFilter::alloc(),
            display,
            excluded_applications,
            &NSArray::new(),
        )
    }
}

fn filter_geometry(
    filter: &SCContentFilter,
    fallback_scale: f64,
    fallback_size: CGSize,
) -> Result<(u32, u32, f64)> {
    let scale = filter_scale(filter, fallback_scale);
    let content_size = if filter.respondsToSelector(sel!(contentRect)) {
        // SAFETY: selector availability was checked on this exact filter.
        unsafe { filter.contentRect().size }
    } else {
        fallback_size
    };
    let (width, height) = dimensions(content_size.width * scale, content_size.height * scale)
        .or_else(|_| dimensions(fallback_size.width * scale, fallback_size.height * scale))?;
    Ok((width, height, scale))
}

fn filter_scale(filter: &SCContentFilter, fallback: f64) -> f64 {
    if !filter.respondsToSelector(sel!(pointPixelScale)) {
        return fallback;
    }
    // SAFETY: selector availability was checked on this exact filter.
    let scale = unsafe { filter.pointPixelScale() as f64 };
    if scale.is_finite() && (0.5..=16.0).contains(&scale) {
        scale
    } else {
        fallback
    }
}

fn current_process_applications(
    content: &SCShareableContent,
) -> Retained<NSArray<SCRunningApplication>> {
    // SAFETY: immutable process identifiers from one retained shareable-content
    // snapshot.
    let applications = unsafe { content.applications() }
        .iter()
        .filter(|application| is_current_process(unsafe { application.processID() }))
        .collect::<Vec<_>>();
    NSArray::from_retained_slice(&applications)
}

fn is_current_process(process_id: i32) -> bool {
    u32::try_from(process_id).is_ok_and(|process_id| process_id == std::process::id())
}

fn display_scale(display: &SCDisplay) -> f64 {
    // SAFETY: immutable display identifier from the shareable-content snapshot.
    let id = unsafe { display.displayID() };
    let Some(mode) = CGDisplayCopyDisplayMode(id) else {
        return 1.0;
    };
    let points = CGDisplayMode::width(Some(&mode));
    let pixels = CGDisplayMode::pixel_width(Some(&mode));
    if points == 0 || pixels == 0 {
        return 1.0;
    }
    let scale = pixels as f64 / points as f64;
    if scale.is_finite() && (0.5..=16.0).contains(&scale) {
        scale
    } else {
        1.0
    }
}

fn window_scale(content: &SCShareableContent, frame: CGRect) -> f64 {
    let centre = (
        frame.origin.x + frame.size.width / 2.0,
        frame.origin.y + frame.size.height / 2.0,
    );
    // SAFETY: immutable reads from the snapshot.
    unsafe {
        content
            .displays()
            .iter()
            .find_map(|display| {
                let bounds = display.frame();
                if contains_point(bounds, centre) && bounds.size.width > 0.0 {
                    Some(display_scale(&display))
                } else {
                    None
                }
            })
            .unwrap_or(1.0)
    }
}

fn dimensions(width: f64, height: f64) -> Result<(u32, u32)> {
    let width = width.round();
    let height = height.round();
    if !width.is_finite()
        || !height.is_finite()
        || width < 1.0
        || height < 1.0
        || width > f64::from(u32::MAX)
        || height > f64::from(u32::MAX)
    {
        return Err(Error::InvalidRequest(
            "the requested recording has invalid pixel dimensions".to_owned(),
        ));
    }
    Ok((width as u32, height as u32))
}

fn logical_rect(rect: CGRect) -> LogicalRect {
    LogicalRect::new(
        LogicalPoint::new(rect.origin.x, rect.origin.y),
        LogicalSize::new(rect.size.width, rect.size.height),
    )
}

fn intersection(left: LogicalRect, right: LogicalRect) -> Option<LogicalRect> {
    let x = left.origin.x.max(right.origin.x);
    let y = left.origin.y.max(right.origin.y);
    let max_x = (left.origin.x + left.size.width).min(right.origin.x + right.size.width);
    let max_y = (left.origin.y + left.size.height).min(right.origin.y + right.size.height);
    (max_x > x && max_y > y).then(|| {
        LogicalRect::new(
            LogicalPoint::new(x, y),
            LogicalSize::new(max_x - x, max_y - y),
        )
    })
}

fn union(left: LogicalRect, right: LogicalRect) -> LogicalRect {
    let x = left.origin.x.min(right.origin.x);
    let y = left.origin.y.min(right.origin.y);
    let max_x = (left.origin.x + left.size.width).max(right.origin.x + right.size.width);
    let max_y = (left.origin.y + left.size.height).max(right.origin.y + right.size.height);
    LogicalRect::new(
        LogicalPoint::new(x, y),
        LogicalSize::new(max_x - x, max_y - y),
    )
}

fn map_rect(
    destination: LogicalRect,
    canvas: LogicalRect,
    output_width: u32,
    output_height: u32,
) -> PixelRect {
    let x = mapped_edge(
        destination.origin.x,
        canvas.origin.x,
        canvas.size.width,
        output_width,
    );
    let y = mapped_edge(
        destination.origin.y,
        canvas.origin.y,
        canvas.size.height,
        output_height,
    );
    let right = mapped_edge(
        destination.origin.x + destination.size.width,
        canvas.origin.x,
        canvas.size.width,
        output_width,
    )
    .max(x.saturating_add(1))
    .min(output_width);
    let bottom = mapped_edge(
        destination.origin.y + destination.size.height,
        canvas.origin.y,
        canvas.size.height,
        output_height,
    )
    .max(y.saturating_add(1))
    .min(output_height);
    PixelRect {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    }
}

fn mapped_edge(value: f64, origin: f64, extent: f64, output_extent: u32) -> u32 {
    (((value - origin) / extent) * f64::from(output_extent))
        .round()
        .clamp(0.0, f64::from(output_extent)) as u32
}

fn contains_point(rect: CGRect, point: (f64, f64)) -> bool {
    point.0 >= rect.origin.x
        && point.1 >= rect.origin.y
        && point.0 < rect.origin.x + rect.size.width
        && point.1 < rect.origin.y + rect.size.height
}

fn contains_rect(display: CGRect, rect: scrozz_core::LogicalRect) -> bool {
    rect.origin.x >= display.origin.x
        && rect.origin.y >= display.origin.y
        && rect.origin.x + rect.size.width <= display.origin.x + display.size.width
        && rect.origin.y + rect.size.height <= display.origin.y + display.size.height
}

fn find_display(content: &SCShareableContent, id: u32) -> Result<Retained<SCDisplay>> {
    // SAFETY: immutable snapshot reads.
    unsafe {
        content
            .displays()
            .iter()
            .find(|display| display.displayID() == id)
    }
    .ok_or_else(|| Error::TargetGone(format!("display {id} is no longer shareable")))
}

fn find_window(content: &SCShareableContent, id: u32) -> Result<Retained<SCWindow>> {
    // SAFETY: immutable snapshot reads.
    unsafe {
        content
            .windows()
            .iter()
            .find(|window| window.windowID() == id)
    }
    .ok_or_else(|| Error::TargetGone(format!("window {id} is no longer open")))
}

fn shareable_content() -> Result<Retained<SCShareableContent>> {
    permission::ensure_screen()?;

    let delivery = Arc::new((Mutex::new(None::<ContentDelivery>), Condvar::new()));
    let handler = {
        let delivery = Arc::clone(&delivery);
        RcBlock::new(
            move |value: *mut SCShareableContent, failure: *mut NSError| {
                // SAFETY: ScreenCaptureKit supplies null or live objects of the
                // callback's declared classes; retaining crosses the pool boundary.
                let result = unsafe {
                    ContentDelivery {
                        content: Retained::retain(value),
                        failure: Retained::retain(failure),
                    }
                };
                let (slot, ready) = &*delivery;
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
                ready.notify_all();
            },
        )
    };

    // SAFETY: arguments match the generated BOOL, BOOL, block signature.
    unsafe {
        SCShareableContent::
            getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                true, false, &handler,
            );
    }

    let (slot, ready) = &*delivery;
    let (mut slot, _) = ready
        .wait_timeout_while(
            slot.lock().unwrap_or_else(PoisonError::into_inner),
            Duration::from_secs(5),
            |value| value.is_none(),
        )
        .unwrap_or_else(PoisonError::into_inner);
    let ContentDelivery {
        content: value,
        failure,
    } = slot.take().ok_or_else(|| {
        Error::Platform("ScreenCaptureKit did not return shareable content in time".to_owned())
    })?;
    value.ok_or_else(|| {
        failure.map_or_else(
            || Error::Platform("ScreenCaptureKit returned no shareable content".to_owned()),
            |failure| error::from_sck(&failure, "listing shareable content"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_exclusion_matches_only_this_nonnegative_pid() {
        let current = i32::try_from(std::process::id()).expect("macOS process ids fit i32");
        assert!(is_current_process(current));
        assert!(!is_current_process(-1));
    }

    #[test]
    fn mixed_scale_tiles_keep_one_global_geometry_without_upscaling() {
        let canvas = LogicalRect::new(
            LogicalPoint::new(-100.0, 0.0),
            LogicalSize::new(300.0, 100.0),
        );
        let retina = LogicalRect::new(
            LogicalPoint::new(-100.0, 0.0),
            LogicalSize::new(100.0, 100.0),
        );
        let external =
            LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(200.0, 100.0));

        assert_eq!(
            map_rect(retina, canvas, 300, 100),
            PixelRect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }
        );
        assert_eq!(
            map_rect(external, canvas, 300, 100),
            PixelRect {
                x: 100,
                y: 0,
                width: 200,
                height: 100,
            }
        );
    }

    #[test]
    fn a_region_crossing_a_display_seam_maps_without_a_gap() {
        let region = LogicalRect::new(LogicalPoint::new(80.0, 10.0), LogicalSize::new(40.0, 50.0));
        let left = intersection(
            LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(100.0, 100.0)),
            region,
        )
        .unwrap();
        let right = intersection(
            LogicalRect::new(
                LogicalPoint::new(100.0, 0.0),
                LogicalSize::new(200.0, 100.0),
            ),
            region,
        )
        .unwrap();

        assert_eq!(
            map_rect(left, region, 40, 50),
            PixelRect {
                x: 0,
                y: 0,
                width: 20,
                height: 50,
            }
        );
        assert_eq!(
            map_rect(right, region, 40, 50),
            PixelRect {
                x: 20,
                y: 0,
                width: 20,
                height: 50,
            }
        );
    }
}
