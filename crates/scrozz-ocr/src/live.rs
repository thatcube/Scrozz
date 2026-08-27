//! Fast OCR helpers for pointer-driven interactions.

use scrozz_core::{Frame, LogicalPoint, LogicalRect, Result};

use crate::{Accuracy, Ocr, Options, SystemOcr, TextBlock};

/// Returns the smallest recognized block containing a frame-local logical point.
#[must_use]
pub fn block_at_point(blocks: &[TextBlock], point: LogicalPoint) -> Option<&TextBlock> {
    blocks
        .iter()
        .filter(|block| {
            let bounds = block.bounds;
            !bounds.is_empty()
                && point.x >= bounds.origin.x
                && point.x <= bounds.origin.x + bounds.size.width
                && point.y >= bounds.origin.y
                && point.y <= bounds.origin.y + bounds.size.height
        })
        .min_by(|left, right| {
            let left_area = left.bounds.size.width * left.bounds.size.height;
            let right_area = right.bounds.size.width * right.bounds.size.height;
            left_area.total_cmp(&right_area)
        })
}

/// Converts a global point into frame-local coordinates when it lies in the frame.
#[must_use]
pub fn frame_local_point(bounds: LogicalRect, global: LogicalPoint) -> Option<LogicalPoint> {
    let right = bounds.origin.x + bounds.size.width;
    let bottom = bounds.origin.y + bounds.size.height;
    (global.x >= bounds.origin.x
        && global.y >= bounds.origin.y
        && global.x <= right
        && global.y <= bottom)
        .then(|| LogicalPoint::new(global.x - bounds.origin.x, global.y - bounds.origin.y))
}

/// A fast-mode recognizer for repeatedly probing text under a pointer.
#[derive(Debug, Clone)]
pub struct LiveOcr {
    ocr: SystemOcr,
}

impl Default for LiveOcr {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveOcr {
    /// Creates a live recognizer using system-language selection.
    #[must_use]
    pub fn new() -> Self {
        Self::with_options(Options::new())
    }

    /// Creates a recognizer that preserves all preferences except accuracy.
    #[must_use]
    pub fn with_options(mut options: Options) -> Self {
        options.accuracy = Accuracy::Fast;
        Self {
            ocr: SystemOcr::with_options(options),
        }
    }

    /// The options in force. Accuracy is always [`Accuracy::Fast`].
    #[must_use]
    pub const fn options(&self) -> &Options {
        self.ocr.options()
    }

    /// Replaces preferences for subsequent pointer probes while retaining fast mode.
    pub fn set_options(&mut self, mut options: Options) {
        options.accuracy = Accuracy::Fast;
        self.ocr.set_options(options);
    }

    /// Recognizes a frame and returns the block under a frame-local logical point.
    ///
    /// # Errors
    ///
    /// Propagates image validation and backend recognition failures.
    pub fn recognize_at(&self, frame: &Frame, point: LogicalPoint) -> Result<Option<TextBlock>> {
        let blocks = self.ocr.recognize(frame)?;
        Ok(block_at_point(&blocks, point).cloned())
    }

    /// Recognizes the block under a global pointer for a frame captured at `bounds`.
    ///
    /// A pointer outside the captured frame returns `None` without invoking OCR.
    ///
    /// # Errors
    ///
    /// Propagates image validation and backend recognition failures.
    pub fn recognize_global_at(
        &self,
        frame: &Frame,
        bounds: LogicalRect,
        pointer: LogicalPoint,
    ) -> Result<Option<TextBlock>> {
        let Some(local) = frame_local_point(bounds, pointer) else {
            return Ok(None);
        };
        self.recognize_at(frame, local)
    }
}

#[cfg(test)]
mod tests {
    use scrozz_core::{LogicalRect, LogicalSize};

    use super::*;

    fn block(text: &str, bounds: LogicalRect) -> TextBlock {
        TextBlock {
            text: text.to_string(),
            confidence: 1.0,
            bounds,
        }
    }

    #[test]
    fn selects_the_smallest_overlapping_block() {
        let blocks = vec![
            block(
                "large",
                LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(100.0, 100.0)),
            ),
            block(
                "word",
                LogicalRect::new(LogicalPoint::new(10.0, 10.0), LogicalSize::new(20.0, 10.0)),
            ),
        ];

        assert_eq!(
            block_at_point(&blocks, LogicalPoint::new(15.0, 15.0)).map(|block| block.text.as_str()),
            Some("word")
        );
        assert!(block_at_point(&blocks, LogicalPoint::new(200.0, 200.0)).is_none());
    }

    #[test]
    fn global_points_are_translated_and_outside_points_are_rejected() {
        let bounds = LogicalRect::new(
            LogicalPoint::new(-100.0, 50.0),
            LogicalSize::new(500.0, 300.0),
        );
        assert_eq!(
            frame_local_point(bounds, LogicalPoint::new(25.0, 80.0)),
            Some(LogicalPoint::new(125.0, 30.0))
        );
        assert_eq!(
            frame_local_point(bounds, LogicalPoint::new(401.0, 80.0)),
            None
        );
    }

    #[test]
    fn live_options_can_be_replaced_without_losing_fast_mode() {
        let mut live = LiveOcr::new();
        live.set_options(
            Options::new()
                .with_languages(["de-DE"])
                .with_accuracy(Accuracy::Accurate)
                .with_line_breaks(crate::LineBreaks::Collapse),
        );

        assert_eq!(live.options().accuracy, Accuracy::Fast);
        assert_eq!(live.options().languages, ["de-DE"]);
        assert_eq!(live.options().line_breaks, crate::LineBreaks::Collapse);
    }
}
