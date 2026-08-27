//! Shared fixtures for the annotation tests.
//!
//! Everything here builds synthetic captures in memory, so the whole suite runs
//! headless with no display, no permissions and no golden files.

#![allow(dead_code)]

use scrozz_annotate::{Annotation, Color, Document, RedactStyle, Style};
use scrozz_core::{
    Capture, CaptureTarget, ColorSpace, DisplayId, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalSize, PixelFormat, Provenance, ScaleFactor, WindowId,
};

/// Builds a frame from a per-pixel colour function.
///
/// The stride is deliberately padded past the minimum so that every test
/// exercises the stride-aware read path — an unpadded fixture would let a
/// stride bug through and produce the classic diagonal skew only on real
/// hardware.
pub fn frame_with<F>(width: u32, height: u32, scale: f64, f: F) -> Frame
where
    F: Fn(u32, u32) -> [u8; 4],
{
    let stride = width as usize * 4 + 12;
    let mut data = vec![0u8; stride * height as usize];
    for y in 0..height {
        for x in 0..width {
            let px = f(x, y);
            let at = y as usize * stride + x as usize * 4;
            data[at..at + 4].copy_from_slice(&px);
        }
    }
    Frame {
        data,
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride,
        format: PixelFormat::Rgba8,
        color_space: ColorSpace::Srgb,
        scale: ScaleFactor::new(scale),
    }
}

/// A high-frequency black/white checkerboard.
///
/// The right fixture for redaction tests: any blur, average or block-fill
/// destroys the pure extremes, so "no pure black and no pure white remain"
/// is a decisive statement that the original pixels are gone.
pub fn checkerboard(width: u32, height: u32, cell: u32) -> Frame {
    frame_with(width, height, 1.0, move |x, y| {
        if (x / cell + y / cell).is_multiple_of(2) {
            [0, 0, 0, 255]
        } else {
            [255, 255, 255, 255]
        }
    })
}

/// A single flat colour.
pub fn flat(width: u32, height: u32, color: [u8; 4]) -> Frame {
    frame_with(width, height, 1.0, move |_, _| color)
}

/// Wraps a frame as a capture with the given provenance.
pub fn capture_with(frame: Frame, provenance: Provenance) -> Capture {
    let target = match provenance {
        Provenance::Window => CaptureTarget::Window(WindowId("test-window".to_owned())),
        Provenance::Region => CaptureTarget::Region(LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(f64::from(frame.width()), f64::from(frame.height())),
        )),
        Provenance::AllDisplays | Provenance::Stitched => CaptureTarget::AllDisplays,
        Provenance::Display => CaptureTarget::Display(DisplayId("test-display".to_owned())),
    };
    Capture {
        frame,
        provenance,
        target,
    }
}

/// A region capture — beautification is permitted.
pub fn region_capture(width: u32, height: u32) -> Capture {
    capture_with(
        flat(width, height, [200, 200, 200, 255]),
        Provenance::Region,
    )
}

/// A window capture — beautification is forbidden by decision D9.
pub fn window_capture(width: u32, height: u32) -> Capture {
    capture_with(
        flat(width, height, [200, 200, 200, 255]),
        Provenance::Window,
    )
}

/// An empty document over a plain region capture.
pub fn document(width: u32, height: u32) -> Document {
    Document::new(region_capture(width, height))
}

/// One of every annotation variant, for round-trip and coverage tests.
pub fn every_annotation() -> Vec<(Annotation, Style)> {
    vec![
        (
            Annotation::Arrow {
                from: LogicalPoint::new(4.5, 6.25),
                to: LogicalPoint::new(88.0, 51.5),
            },
            Style::stroked()
                .with_stroke(Color::ACCENT)
                .with_stroke_width(3.5),
        ),
        (
            Annotation::Line {
                from: LogicalPoint::new(12.0, 7.0),
                to: LogicalPoint::new(91.0, 34.0),
            },
            Style::stroked()
                .with_stroke(Color::rgb(61, 139, 255))
                .with_stroke_width(2.0),
        ),
        (
            Annotation::Rectangle(rect(10.0, 12.0, 40.0, 22.0)),
            Style::stroked()
                .with_stroke(Color::rgb(20, 160, 90))
                .with_fill(Some(Color::rgba(20, 160, 90, 64))),
        ),
        (
            Annotation::Ellipse(rect(50.0, 8.0, 30.0, 30.0)),
            Style::stroked().with_opacity(0.5),
        ),
        (
            Annotation::Freehand(vec![
                LogicalPoint::new(2.0, 2.0),
                LogicalPoint::new(9.5, 14.25),
                LogicalPoint::new(21.0, 6.75),
                LogicalPoint::new(33.5, 19.0),
            ]),
            Style::stroked().with_stroke_width(2.25),
        ),
        (
            Annotation::Text {
                at: LogicalPoint::new(6.0, 70.0),
                content: "Ship it! 42%".to_owned(),
            },
            Style::stroked().with_font_size(21.5),
        ),
        (
            Annotation::Counter {
                at: LogicalPoint::new(70.0, 70.0),
                index: 1,
            },
            Style::stroked().with_fill(Some(Color::ACCENT)),
        ),
        (
            Annotation::Highlight(rect(4.0, 40.0, 60.0, 12.0)),
            Style::highlighter(),
        ),
        (
            Annotation::Spotlight(rect(18.0, 16.0, 55.0, 38.0)),
            Style::spotlight(),
        ),
        (
            Annotation::Redact {
                area: rect(20.0, 55.0, 25.0, 10.0),
                style: RedactStyle::Blur,
            },
            Style::redaction(),
        ),
        (
            Annotation::Redact {
                area: rect(46.0, 55.0, 25.0, 10.0),
                style: RedactStyle::Pixelate,
            },
            Style::redaction(),
        ),
        (
            Annotation::Redact {
                area: rect(72.0, 55.0, 20.0, 10.0),
                style: RedactStyle::Solid,
            },
            Style::redaction().with_fill(Some(Color::rgb(9, 9, 9))),
        ),
    ]
}

/// A logical rectangle from origin and size.
pub fn rect(x: f64, y: f64, w: f64, h: f64) -> LogicalRect {
    LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(w, h))
}

/// Reads a rendered frame's pixel as straight (un-premultiplied) RGBA.
///
/// Rendered output is premultiplied, so comparing it to authored colours needs
/// this un-doing first.
#[must_use]
pub fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
    let at = y as usize * frame.stride + x as usize * 4;
    let raw = &frame.data[at..at + 4];
    let a = raw[3];
    if a == 0 {
        return [0, 0, 0, 0];
    }
    let un = |c: u8| ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8;
    [un(raw[0]), un(raw[1]), un(raw[2]), a]
}

/// Every pixel of a rendered frame as straight RGBA, row-major, stride removed.
#[must_use]
pub fn pixels(frame: &Frame) -> Vec<[u8; 4]> {
    let mut out = Vec::with_capacity((frame.width() * frame.height()) as usize);
    for y in 0..frame.height() {
        for x in 0..frame.width() {
            out.push(pixel(frame, x, y));
        }
    }
    out
}

/// Whether a pixel is close to a colour, allowing for antialiasing and rounding.
#[must_use]
pub fn near(a: [u8; 4], b: [u8; 4], tolerance: u8) -> bool {
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| x.abs_diff(*y) <= tolerance)
}
