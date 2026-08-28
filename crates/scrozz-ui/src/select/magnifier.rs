use scrozz_core::{DisplayId, LogicalPoint, ScaleFactor};

use super::frozen::{FrozenDisplayFrame, FrozenPixel};

/// Magnifier defaults used by the selector overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MagnifierConfig {
    /// Physical pixels per magnified pixel.
    pub zoom: u32,
    /// Number of sampled source pixels on each side.
    pub side: usize,
}

impl Default for MagnifierConfig {
    fn default() -> Self {
        Self { zoom: 5, side: 32 }
    }
}

/// One pixel in the magnifier grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MagnifierCell {
    /// X coordinate in the source frame.
    pub x: u32,
    /// Y coordinate in the source frame.
    pub y: u32,
    /// Pixel colour.
    pub pixel: FrozenPixel,
}

/// A pure sampled grid for painting a pixel loupe.
#[derive(Debug, Clone, PartialEq)]
pub struct MagnifierGrid {
    /// Owning display.
    pub display: DisplayId,
    /// Display scale used when translating from logical to physical coordinates.
    pub scale: ScaleFactor,
    /// Physical source pixel under the pointer.
    pub focus_px: (u32, u32),
    /// Sampled cells, row-major.
    pub cells: Vec<MagnifierCell>,
    /// Grid side length.
    pub side: usize,
    /// Physical pixels per magnified pixel.
    pub zoom: u32,
}

impl MagnifierGrid {
    /// The centre cell index.
    #[must_use]
    pub fn centre_index(&self) -> usize {
        let centre = self.side / 2;
        centre * self.side + centre
    }

    /// The centre sampled cell.
    #[must_use]
    pub fn centre(&self) -> MagnifierCell {
        self.cells[self.centre_index()]
    }
}

/// Samples a magnifier grid from the owning frozen display.
#[must_use]
pub fn sample(
    frame: &FrozenDisplayFrame,
    point: LogicalPoint,
    config: MagnifierConfig,
) -> MagnifierGrid {
    let (focus_x, focus_y) = super::geom::logical_to_local_physical(&frame.display, point);
    let side = config.side.max(1);
    let half = side / 2;
    let mut cells = Vec::with_capacity(side * side);
    for dy in 0..side {
        for dx in 0..side {
            let px = focus_x as i64 + dx as i64 - half as i64;
            let py = focus_y as i64 + dy as i64 - half as i64;
            let px = px.clamp(0, frame.width().saturating_sub(1) as i64) as u32;
            let py = py.clamp(0, frame.height().saturating_sub(1) as i64) as u32;
            cells.push(MagnifierCell {
                x: px,
                y: py,
                pixel: frame.sample_local(px, py),
            });
        }
    }
    MagnifierGrid {
        display: frame.display.id.clone(),
        scale: frame.display.scale,
        focus_px: (focus_x, focus_y),
        cells,
        side,
        zoom: config.zoom.max(1),
    }
}
