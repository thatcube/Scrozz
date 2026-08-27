//! Coordinate geometry, in two deliberately incompatible spaces.
//!
//! # Why this module exists
//!
//! A screenshot tool lives or dies on the difference between the coordinates a
//! user *sees* and the pixels a display *has*. On a Retina Mac a region dragged
//! out as 1920×1080 is 3840×2160 real pixels. On a mixed-DPI Windows desktop,
//! one monitor may be 1.0× and the next 1.5×, so a rectangle that spans both has
//! no single scale factor at all. On Wayland, fractional scaling means the factor
//! need not even be an integer.
//!
//! Almost every visible bug in this class of app — a blurry capture, a region
//! that grabs slightly the wrong crop, an annotation that drifts from what it
//! points at, a "2× bigger than expected" PNG — comes from adding a logical
//! number to a physical one. The types below make that a compile error rather
//! than a bug report.
//!
//! ## The rule
//!
//! - **Logical** ([`LogicalRect`] and friends) is UI space: points, what the user
//!   drags, what a window reports as its size, what an annotation is authored in.
//! - **Physical** ([`PhysicalRect`] and friends) is pixel space: what a capture
//!   backend returns, what gets encoded into a file.
//!
//! They do not mix, and there is exactly one bridge: [`ScaleFactor`]. Every
//! conversion is therefore explicit and greppable.

use std::marker::PhantomData;

use serde::{Deserialize, Serialize};

/// Marker for UI coordinates — points, not pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Logical;

/// Marker for device coordinates — real pixels in a framebuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Physical;

/// The ratio of physical pixels to logical points, e.g. `2.0` on Retina.
///
/// Deliberately `f64`: Wayland and Windows both permit fractional scaling
/// (1.25, 1.5, 1.75), so an integer type here would be wrong on two of our
/// three platforms.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ScaleFactor(f64);

impl ScaleFactor {
    /// A 1:1 display: one point is one pixel.
    pub const IDENTITY: Self = Self(1.0);

    /// Creates a scale factor.
    ///
    /// # Panics
    ///
    /// Panics if `factor` is not finite and strictly positive. A non-positive or
    /// `NaN` scale factor is always a bug in the caller — silently clamping it
    /// would push a wrong capture size downstream where it is far harder to
    /// diagnose.
    #[must_use]
    pub fn new(factor: f64) -> Self {
        assert!(
            factor.is_finite() && factor > 0.0,
            "scale factor must be finite and positive, got {factor}"
        );
        Self(factor)
    }

    /// The raw ratio.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl Default for ScaleFactor {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// A point in coordinate space `S`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point<S> {
    /// Horizontal offset, increasing rightwards.
    pub x: f64,
    /// Vertical offset, increasing downwards.
    pub y: f64,
    #[serde(skip)]
    _space: PhantomData<S>,
}

/// A size in coordinate space `S`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Size<S> {
    /// Extent along x. Never negative.
    pub width: f64,
    /// Extent along y. Never negative.
    pub height: f64,
    #[serde(skip)]
    _space: PhantomData<S>,
}

/// An axis-aligned rectangle in coordinate space `S`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Rect<S> {
    /// Top-left corner.
    pub origin: Point<S>,
    /// Extent from the origin.
    pub size: Size<S>,
}

impl<S> Point<S> {
    /// Creates a point.
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self {
            x,
            y,
            _space: PhantomData,
        }
    }
}

impl<S> Size<S> {
    /// Creates a size.
    ///
    /// Negative extents are clamped to zero; an inverted size is meaningless and
    /// callers routinely produce one by dragging a selection up and to the left.
    #[must_use]
    pub fn new(width: f64, height: f64) -> Self {
        Self {
            width: width.max(0.0),
            height: height.max(0.0),
            _space: PhantomData,
        }
    }

    /// Whether this size encloses no area.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }
}

impl<S> Rect<S> {
    /// Creates a rectangle from an origin and a size.
    #[must_use]
    pub const fn new(origin: Point<S>, size: Size<S>) -> Self {
        Self { origin, size }
    }

    /// Creates a rectangle from two opposite corners, in any order.
    ///
    /// This is the constructor a drag-to-select gesture wants: the user may drag
    /// in any of four directions and all of them must yield a valid rectangle.
    #[must_use]
    pub fn from_corners(a: Point<S>, b: Point<S>) -> Self {
        let origin = Point::new(a.x.min(b.x), a.y.min(b.y));
        let size = Size::new((a.x - b.x).abs(), (a.y - b.y).abs());
        Self { origin, size }
    }

    /// Whether this rectangle encloses no area.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.size.is_empty()
    }
}

/// A point in UI space.
pub type LogicalPoint = Point<Logical>;
/// A size in UI space.
pub type LogicalSize = Size<Logical>;
/// A rectangle in UI space.
pub type LogicalRect = Rect<Logical>;

/// A point in device-pixel space.
pub type PhysicalPoint = Point<Physical>;
/// A size in device-pixel space.
pub type PhysicalSize = Size<Physical>;
/// A rectangle in device-pixel space.
pub type PhysicalRect = Rect<Physical>;

impl LogicalRect {
    /// Converts to device pixels at the given scale.
    ///
    /// Rounds outwards, never inwards: a selection must never lose an edge pixel
    /// the user included. Cropping one pixel short is visible; including one
    /// extra is not.
    #[must_use]
    pub fn to_physical(self, scale: ScaleFactor) -> PhysicalRect {
        let s = scale.get();
        let left = (self.origin.x * s).floor();
        let top = (self.origin.y * s).floor();
        let right = ((self.origin.x + self.size.width) * s).ceil();
        let bottom = ((self.origin.y + self.size.height) * s).ceil();
        PhysicalRect::new(
            PhysicalPoint::new(left, top),
            PhysicalSize::new(right - left, bottom - top),
        )
    }
}

impl PhysicalRect {
    /// Converts to UI space at the given scale.
    #[must_use]
    pub fn to_logical(self, scale: ScaleFactor) -> LogicalRect {
        let s = scale.get();
        LogicalRect::new(
            LogicalPoint::new(self.origin.x / s, self.origin.y / s),
            LogicalSize::new(self.size.width / s, self.size.height / s),
        )
    }

    /// Width in whole pixels, for buffer allocation.
    #[must_use]
    pub fn pixel_width(&self) -> u32 {
        self.size.width.round() as u32
    }

    /// Height in whole pixels, for buffer allocation.
    #[must_use]
    pub fn pixel_height(&self) -> u32 {
        self.size.height.round() as u32
    }
}
