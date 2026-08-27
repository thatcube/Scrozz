//! Scrozz's user interface: surfaces, design tokens, motion, and the screenshot
//! harness.
//!
//! # The capture stack
//!
//! The primary surface, and per decision D12 the primary interface of the whole
//! app. It is a bottom-left anchored pile of capture cards that grows **upward**:
//! the newest capture is on top, the oldest at the bottom. Cards only ever move
//! downward — when the oldest exits at the bottom, everything above falls one
//! slot, like a stack settling under gravity. New cards enter from the left at
//! their destination height.
//!
//! Direction carries meaning (D21): **left dismisses, right or up begins a drag
//! onto another app, down collapses the pile into the capture dock** (D20).
//!
//! # Motion
//!
//! Per decision D19 motion applies to *objects*, not controls. Cards animate;
//! buttons change state instantly, because instant feedback reads as more
//! responsive, not less.
//!
//! # Screenshots are generated
//!
//! Per decision D25 no product screenshot is ever taken by hand. [`harness`]
//! renders any surface headlessly, with a fixed seed and a virtual clock, so the
//! same code produces golden-image tests, store assets and README imagery. The
//! virtual clock is what makes a motion-heavy UI testable at all: a test renders
//! a *named instant* such as "card entry at 180 ms" rather than whichever frame
//! it happened to catch.

#![forbid(unsafe_code)]

pub mod harness {
    //! Deterministic headless rendering of UI surfaces.

    use scrozz_core::Result;

    /// A named, seeded scenario to render.
    ///
    /// The same list serves golden tests and marketing, so a scenario is
    /// maintained once and cannot drift between them.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Scenario {
        /// A full pile of six capture cards.
        StackFull,
        /// A single capture card.
        StackSingle,
        /// The pile collapsed into the capture dock.
        DockCollapsed,
        /// The annotation toolbar open over a capture.
        EditorAnnotating,
    }

    /// What a render is for, which decides scale and decoration.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Profile {
        /// Committed baseline for pixel-diff tests in CI.
        Golden,
        /// Exact per-store dimensions for an app store listing.
        Store {
            /// Target width in pixels.
            width: u32,
            /// Target height in pixels.
            height: u32,
        },
        /// Documentation and README imagery.
        Docs,
    }

    /// Renders a scenario to PNG bytes at a fixed instant.
    ///
    /// `at_ms` drives the virtual clock, so any frame of any animation is
    /// reproducible. Stepping it and encoding the sequence is how animated store
    /// previews and README captures are produced.
    ///
    /// # Errors
    ///
    /// Returns an error if the surface could not be rendered headlessly.
    pub fn render(scenario: &Scenario, profile: &Profile, at_ms: u64) -> Result<Vec<u8>> {
        todo!("render the surface headlessly at a fixed virtual instant")
    }
}
