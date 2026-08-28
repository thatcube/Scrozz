//! Selector-focused regression tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scrozz_core::{
    ColorSpace, Display, DisplayId, Frame, LogicalPoint, LogicalRect, LogicalSize, PhysicalSize,
    PixelFormat, ScaleFactor,
};
use scrozz_ui::{FrozenDisplayFrame, FrozenPixel, MagnifierConfig};

fn display(scale: f64) -> Display {
    let bounds = LogicalRect::new(LogicalPoint::new(10.0, 20.0), LogicalSize::new(2.0, 1.0));
    Display {
        id: DisplayId("main".to_owned()),
        name: "main".to_owned(),
        bounds,
        work_area: bounds,
        scale: ScaleFactor::new(scale),
        is_primary: true,
    }
}

fn frame(format: PixelFormat, data: Vec<u8>, stride: usize) -> Frame {
    Frame {
        data,
        size: PhysicalSize::new(2.0, 1.0),
        stride,
        format,
        color_space: ColorSpace::Srgb,
        scale: ScaleFactor::new(2.0),
    }
}

#[test]
fn conversion_honours_stride_and_unpremultiplies() {
    let straight = FrozenDisplayFrame::from_frame(
        display(2.0),
        frame(
            PixelFormat::BgraPremultiplied8,
            vec![20, 40, 60, 128, 0, 0, 128, 128, 0, 0, 0, 0],
            12,
        ),
    )
    .unwrap();

    assert_eq!(
        straight.sample_local(0, 0),
        FrozenPixel {
            r: 120,
            g: 80,
            b: 40,
            a: 128
        }
    );
    assert_eq!(
        straight.sample_local(1, 0),
        FrozenPixel {
            r: 255,
            g: 0,
            b: 0,
            a: 128
        }
    );
}

#[test]
fn every_pixel_format_round_trips_to_a_color_image() {
    for format in [
        PixelFormat::Rgba8,
        PixelFormat::Bgra8,
        PixelFormat::RgbaPremultiplied8,
        PixelFormat::BgraPremultiplied8,
    ] {
        let bytes = match format {
            PixelFormat::Rgba8 => vec![10, 20, 30, 255, 40, 50, 60, 255],
            PixelFormat::Bgra8 => vec![30, 20, 10, 255, 60, 50, 40, 255],
            PixelFormat::RgbaPremultiplied8 => vec![10, 20, 30, 255, 40, 50, 60, 255],
            PixelFormat::BgraPremultiplied8 => vec![30, 20, 10, 255, 60, 50, 40, 255],
        };
        let frozen = FrozenDisplayFrame::from_frame(display(2.0), frame(format, bytes, 8)).unwrap();
        let image = frozen.color_image();
        assert_eq!(image.size, [2, 1]);
    }
}

#[test]
fn global_logical_sampling_uses_the_display_scale() {
    let frozen = FrozenDisplayFrame::synthetic(display(2.0), 7);
    let expected = frozen.sample_local(1, 0);
    let sampled = frozen.sample_global_logical(LogicalPoint::new(10.5, 20.0));
    assert_eq!(sampled, expected);
}

#[test]
fn magnifier_uses_a_crisp_thirty_two_pixel_grid_at_five_x() {
    let bounds = LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(64.0, 64.0));
    let display = Display {
        id: DisplayId("loupe".to_owned()),
        name: "loupe".to_owned(),
        bounds,
        work_area: bounds,
        scale: ScaleFactor::IDENTITY,
        is_primary: true,
    };
    let frozen = FrozenDisplayFrame::synthetic(display, 9);
    let grid = scrozz_ui::select::magnifier::sample(
        &frozen,
        LogicalPoint::new(32.0, 32.0),
        MagnifierConfig::default(),
    );

    assert_eq!(grid.side, 32);
    assert_eq!(grid.zoom, 5);
    assert_eq!(grid.cells.len(), 32 * 32);
    assert_eq!(grid.centre().x, grid.focus_px.0);
    assert_eq!(grid.centre().y, grid.focus_px.1);
}
