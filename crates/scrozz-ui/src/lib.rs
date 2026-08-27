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
//! # Layers
//!
//! The crate is built in layers, each depending only on the ones above it:
//!
//! | Layer | Module | What it owns |
//! |---|---|---|
//! | Tokens | [`theme`] | Colour, space, radius, elevation, type |
//! | Time | [`motion`] | Durations, easing, springs, the virtual clock |
//! | Assets | [`icons`] | SVG → texture, rasterised once per context |
//! | Platform | [`vibrancy`] | OS window materials, where they exist |
//! | Drawing | [`paint`] | Primitives and controls built from all of the above |
//! | Surfaces | [`stack`] | The product's actual screens |
//! | Verification | [`harness`] | Headless rendering of any surface |
//!
//! # Motion
//!
//! Per decision D19 motion applies to *objects*, not controls. Cards animate;
//! buttons change state instantly, because instant feedback reads as more
//! responsive, not less. [`paint`]'s controls therefore contain no animation at
//! all — by design, not by omission.
//!
//! Per decision D13 the OS reduce-motion setting collapses every duration to
//! zero through a single choke point, [`motion::Motion::resolve`]. There is no
//! second path by which a duration can reach a curve.
//!
//! # Screenshots are generated
//!
//! Per decision D25 no product screenshot is ever taken by hand. [`harness`]
//! renders any surface headlessly, with a fixed seed and a virtual clock, so the
//! same code produces golden-image tests, store assets and README imagery. The
//! virtual clock is what makes a motion-heavy UI testable at all: a test renders
//! a *named instant* such as "card entry at 180 ms" rather than whichever frame
//! it happened to catch.
//!
//! That is a constraint on the whole crate, not a feature of the harness:
//! **no drawing code may read a clock.** Time arrives as [`motion::Motion`], a
//! value passed down the call tree, and a surface given the same `Motion` and
//! the same state paints the same pixels every time.

#![forbid(unsafe_code)]

pub mod harness;
pub mod icons;
pub mod motion;
pub mod paint;
pub mod stack;
pub mod theme;
pub mod vibrancy;

pub use motion::{Activity, Duration, Ease, Motion, MotionPrefs};
pub use theme::{Appearance, Elevation, Palette, Radius, Space, Text, Theme};
