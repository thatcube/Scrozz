//! Scrolling capture: frame alignment and stitching.
//!
//! Assembles one tall image from a sequence of frames taken while the user
//! scrolls. The hard part is not the seam but knowing *how far* the content
//! moved between frames, which must be recovered from the pixels themselves
//! because no platform reports it.
//!
//! Sticky headers and footers are the characteristic failure: a toolbar pinned
//! to the top of the page does not scroll with the content, so naive alignment
//! either repeats it down the whole stitched image or drags the alignment off.

#![forbid(unsafe_code)]

use scrozz_core::{Frame, Result};

/// Assembles overlapping frames into one tall image.
pub trait Stitcher {
    /// Adds a frame to the sequence.
    ///
    /// # Errors
    ///
    /// Returns an error if the frame does not overlap the previous one enough to
    /// align, which is what a too-fast scroll produces.
    fn push(&mut self, frame: Frame) -> Result<()>;

    /// Produces the assembled image.
    ///
    /// # Errors
    ///
    /// Returns an error if no frames were pushed, or alignment failed.
    fn finish(self: Box<Self>) -> Result<Frame>;
}
