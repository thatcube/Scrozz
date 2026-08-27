//! Shared types, errors and trait contracts for Scrozz.
//!
//! This crate is the architectural spine. It depends on no other Scrozz crate,
//! and every other crate depends on it. That is what lets the rest of the
//! workspace be built independently and in parallel: a crate is implemented
//! against the traits here, not against its neighbours.
//!
//! Nothing in this crate touches the operating system, the filesystem or the
//! network. It is pure types and contracts, so it compiles and tests anywhere,
//! including in headless CI with no display server.
//!
//! # Where the decisions live
//!
//! Several types here exist to make a project decision unrepresentable rather
//! than merely documented. Each is annotated at its definition, and the
//! authoritative record is `docs/decisions.md`:
//!
//! - [`geometry`] separates logical from physical pixels so they cannot be
//!   added together.
//! - [`capture::Provenance`] carries D9 — window captures are never composited
//!   onto — with the image, for the image's whole life.
//! - [`Error::PermissionDenied`] and [`Error::Unsupported`] make D15's
//!   permission-on-first-use and D8's documented platform gaps ordinary,
//!   handled outcomes rather than crashes.

#![forbid(unsafe_code)]

pub mod capture;
pub mod error;
pub mod frame;
pub mod geometry;
pub mod target;

pub use capture::{Capture, CaptureBackend, CaptureRequest, CursorMode, Provenance};
pub use error::{Error, Result};
pub use frame::{ColorSpace, Frame, PixelFormat};
pub use geometry::{
    Logical, LogicalPoint, LogicalRect, LogicalSize, Physical, PhysicalPoint, PhysicalRect,
    PhysicalSize, Point, Rect, ScaleFactor, Size,
};
pub use target::{
    CaptureTarget, Display, DisplayId, TargetEnumerator, Window, WindowId,
};
