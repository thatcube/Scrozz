#![allow(dead_code)]

use std::{collections::VecDeque, time::Duration};

use scrozz_core::{
    ColorSpace, Error, Frame, LogicalPoint, PhysicalSize, PixelFormat, Result, ScaleFactor,
    ScrollAxis, ScrollCapabilities, ScrollDriver, ScrollGesture,
};
use scrozz_stitch::{
    AlignmentConfig, ChromeConfig, FrameSource, ScrollSessionConfig, StitchConfig,
};

pub fn gray_frame(rows: &[u8], width: u32, scale: f64) -> Frame {
    let mut data = Vec::with_capacity(rows.len() * width as usize * 4);
    for &value in rows {
        for x in 0..width {
            let detail = value.wrapping_add((x % 5) as u8);
            data.extend_from_slice(&[detail, detail, detail, 255]);
        }
    }
    Frame {
        data,
        size: PhysicalSize::new(f64::from(width), rows.len() as f64),
        stride: width as usize * 4,
        format: PixelFormat::Rgba8,
        color_space: ColorSpace::Srgb,
        scale: ScaleFactor::new(scale),
    }
}

pub fn viewport(
    document: &[u8],
    start: usize,
    content_rows: usize,
    sticky_top: &[u8],
    sticky_bottom: &[u8],
    width: u32,
    scale: f64,
) -> Frame {
    let mut rows = sticky_top.to_vec();
    rows.extend_from_slice(&document[start..start + content_rows]);
    rows.extend_from_slice(sticky_bottom);
    gray_frame(&rows, width, scale)
}

pub fn gray_column_frame(columns: &[u8], height: u32, scale: f64) -> Frame {
    let width = columns.len() as u32;
    let mut data = Vec::with_capacity(columns.len() * height as usize * 4);
    for y in 0..height {
        for &value in columns {
            let detail = value.wrapping_add((y % 5) as u8);
            data.extend_from_slice(&[detail, detail, detail, 255]);
        }
    }
    Frame {
        data,
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride: width as usize * 4,
        format: PixelFormat::Rgba8,
        color_space: ColorSpace::Srgb,
        scale: ScaleFactor::new(scale),
    }
}

pub fn horizontal_viewport(
    document: &[u8],
    start: usize,
    content_columns: usize,
    sticky_left: &[u8],
    sticky_right: &[u8],
    height: u32,
    scale: f64,
) -> Frame {
    let mut columns = sticky_left.to_vec();
    columns.extend_from_slice(&document[start..start + content_columns]);
    columns.extend_from_slice(sticky_right);
    gray_column_frame(&columns, height, scale)
}

pub fn compact_stitch(expected_delta: Option<u32>) -> StitchConfig {
    StitchConfig {
        alignment: AlignmentConfig {
            min_overlap: 3,
            row_buckets: 6,
            top_k: 6,
            basin_radius: 1,
            min_stationary_edge: 2,
            max_mean_error: 24,
            min_confidence: 1,
            ..AlignmentConfig::default()
        },
        chrome: ChromeConfig {
            min_band: 2,
            ..ChromeConfig::default()
        },
        expected_delta,
        ..StitchConfig::default()
    }
}

pub fn session_config(amount: f64, max_frames: usize) -> ScrollSessionConfig {
    let mut config =
        ScrollSessionConfig::new(ScrollGesture::down(LogicalPoint::new(50.0, 50.0), amount));
    config.max_frames = max_frames;
    config.settle_delay = Duration::ZERO;
    config.manual_poll_interval = Duration::ZERO;
    config.stitch = compact_stitch(None);
    config
}

pub fn horizontal_session_config(amount: f64, max_frames: usize) -> ScrollSessionConfig {
    let mut config = ScrollSessionConfig::new(ScrollGesture {
        axis: ScrollAxis::Horizontal,
        at: LogicalPoint::new(50.0, 50.0),
        display: None,
        window: None,
        owner_pid: None,
        window_bounds: None,
        area: None,
        amount,
    });
    config.max_frames = max_frames;
    config.settle_delay = Duration::ZERO;
    config.manual_poll_interval = Duration::ZERO;
    config.stitch = compact_stitch(None);
    config
}

pub struct FixtureSource {
    pub frames: VecDeque<Frame>,
}

impl FrameSource for FixtureSource {
    fn capture_frame(&mut self) -> Result<Frame> {
        self.frames
            .pop_front()
            .ok_or_else(|| Error::Platform("deterministic fixture ran out of frames".to_owned()))
    }

    fn name(&self) -> &str {
        "deterministic-fixture"
    }
}

#[derive(Default)]
pub struct FixtureDriver;

impl ScrollDriver for FixtureDriver {
    fn capabilities(&self) -> ScrollCapabilities {
        ScrollCapabilities::automatic(false)
    }

    fn prepare(&mut self) -> Result<()> {
        Ok(())
    }

    fn scroll(&mut self, _gesture: &ScrollGesture) -> Result<()> {
        Ok(())
    }

    fn name(&self) -> &str {
        "fixture-driver"
    }
}
