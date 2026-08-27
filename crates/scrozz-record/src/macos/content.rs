//! Resolve a public capture target into one ScreenCaptureKit content filter.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{SCContentFilter, SCDisplay, SCShareableContent, SCWindow};
use scrozz_core::{CaptureTarget, Error, Result};

use super::{error, permission};

pub(crate) struct CaptureContent {
    pub(crate) filter: Retained<SCContentFilter>,
    pub(crate) source_rect: Option<CGRect>,
    pub(crate) native_width: u32,
    pub(crate) native_height: u32,
    pub(crate) scale: f64,
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
    match target {
        CaptureTarget::Display(id) => {
            let display_id = id.0.parse::<u32>().map_err(|_| {
                Error::InvalidRequest(format!("{:?} is not a macOS display id", id.0))
            })?;
            let display = find_display(&content, display_id)?;
            whole_display(&display)
        }
        CaptureTarget::Window(id) => {
            let window_id = id.0.parse::<u32>().map_err(|_| {
                Error::InvalidRequest(format!("{:?} is not a macOS window id", id.0))
            })?;
            let window = find_window(&content, window_id)?;
            window_content(&content, &window)
        }
        CaptureTarget::Region(rect) => region_content(&content, *rect),
        CaptureTarget::AllDisplays => {
            let displays = unsafe { content.displays() };
            match displays.len() {
                0 => Err(Error::Unsupported {
                    what: "recording all displays".to_owned(),
                    why: "no shareable displays are attached".to_owned(),
                }),
                1 => whole_display(&displays.objectAtIndex(0)),
                _ => Err(Error::Unsupported {
                    what: "recording all displays into one video".to_owned(),
                    why: "ScreenCaptureKit streams one display per content filter; select a \
                          display instead of silently recording only one"
                        .to_owned(),
                }),
            }
        }
    }
}

fn whole_display(display: &SCDisplay) -> Result<CaptureContent> {
    // SAFETY: this is ScreenCaptureKit's designated whole-display filter
    // initializer and the exclusion list remains alive for the call.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            display,
            &NSArray::new(),
        )
    };
    let fallback_scale = display_scale(display);
    let (native_width, native_height, scale) = filter_geometry(&filter, fallback_scale)?;
    Ok(CaptureContent {
        filter,
        source_rect: None,
        native_width,
        native_height,
        scale,
    })
}

fn window_content(content: &SCShareableContent, window: &SCWindow) -> Result<CaptureContent> {
    // SAFETY: this is the designated independent-window initializer.
    let filter = unsafe {
        SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), window)
    };
    // SAFETY: immutable geometry reads from the shareable-content snapshot.
    let frame = unsafe { window.frame() };
    let fallback_scale = window_scale(content, frame);
    let (native_width, native_height, scale) = filter_geometry(&filter, fallback_scale)?;
    Ok(CaptureContent {
        filter,
        source_rect: None,
        native_width,
        native_height,
        scale,
    })
}

fn region_content(
    content: &SCShareableContent,
    rect: scrozz_core::LogicalRect,
) -> Result<CaptureContent> {
    let displays = unsafe { content.displays() };
    let display = displays
        .iter()
        .find(|display| {
            // SAFETY: immutable geometry read from the snapshot.
            contains_rect(unsafe { display.frame() }, rect)
        })
        .ok_or_else(|| Error::Unsupported {
            what: "recording this region".to_owned(),
            why: "a live ScreenCaptureKit region must fit entirely on one display".to_owned(),
        })?;
    // SAFETY: immutable geometry read.
    let display_frame = unsafe { display.frame() };
    let source_rect = CGRect::new(
        CGPoint::new(
            rect.origin.x - display_frame.origin.x,
            rect.origin.y - display_frame.origin.y,
        ),
        CGSize::new(rect.size.width, rect.size.height),
    );
    // SAFETY: a whole-display filter is required for region streams; sourceRect
    // performs the crop in the display's local point coordinate space.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &NSArray::new(),
        )
    };
    let scale = filter_scale(&filter, display_scale(&display));
    let (native_width, native_height) =
        dimensions(rect.size.width * scale, rect.size.height * scale)?;
    Ok(CaptureContent {
        filter,
        source_rect: Some(source_rect),
        native_width,
        native_height,
        scale,
    })
}

fn filter_geometry(filter: &SCContentFilter, fallback_scale: f64) -> Result<(u32, u32, f64)> {
    let scale = filter_scale(filter, fallback_scale);
    // SAFETY: immutable geometry read from a configured content filter.
    let content = unsafe { filter.contentRect() };
    let (width, height) = dimensions(content.size.width * scale, content.size.height * scale)?;
    Ok((width, height, scale))
}

fn filter_scale(filter: &SCContentFilter, fallback: f64) -> f64 {
    // SAFETY: immutable scale read from a configured content filter.
    let scale = unsafe { filter.pointPixelScale() as f64 };
    if scale.is_finite() && (0.5..=16.0).contains(&scale) {
        scale
    } else {
        fallback
    }
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
