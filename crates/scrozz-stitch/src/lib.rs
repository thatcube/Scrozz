//! Scrolling capture: deterministic frame alignment, stitching and orchestration.
//!
//! Alignment is intentionally integer-only. The same frame sequence must produce
//! the same seams on every supported architecture or the golden fixtures become
//! noise instead of a contract.

#![forbid(unsafe_code)]

use scrozz_core::{Frame, Result};

pub mod align;
pub mod chrome;
pub mod luma;
pub mod session;
pub mod stitch;

pub use align::{
    AlignError, Alignment, AlignmentConfig, AnalysisBand, AnalysisSpan, align_axis, align_axis_in,
    align_horizontal, align_horizontal_in, align_vertical, align_vertical_in,
};
pub use chrome::{
    ChromeBands, ChromeConfig, SideChromeBands, conservative_chrome, conservative_side_chrome,
    detect_sticky_chrome, detect_sticky_side_chrome,
};
pub use luma::{ColumnProfile, LumaPlane, RowProfile};
pub use session::{
    AtomicCancellation, BackendFrameSource, CancelAction, CancelSignal, CompletionReason,
    FrameSource, NeverCancel, NoopPacer, Pacer, Progress, ScrollDirectionAmounts, ScrollSession,
    ScrollSessionConfig, SessionOutput, ThreadPacer,
};
pub use stitch::{
    PushOutcome, ScrollStitcher, SeamQuality, StitchConfig, StitchSummary, StopReason,
    detect_scroll_direction,
};

/// Assembles overlapping frames into one long image.
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
