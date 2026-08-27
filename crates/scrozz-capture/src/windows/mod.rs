//! Windows still capture.
//!
//! Two paths, chosen once at construction:
//!
//! - **[`wgc`] — `Windows.Graphics.Capture`.** The correct one. Reads the
//!   composed DWM surface, so a window capture contains that window and nothing
//!   else, hardware-accelerated content included, with real alpha at the
//!   rounded corners. Needs Windows 10 1903.
//! - **[`gdi`] — `BitBlt`/`PrintWindow`.** The fallback, with the compromises
//!   documented on that module. Present so that an old or unusual machine still
//!   takes a screenshot rather than showing an error.
//!
//! Everything that can be reasoned about without a Windows machine lives in
//! [`geom`], [`filter`] and [`pixels`], which name no Windows types at all and
//! are unit-tested on every platform. The modules that must call the OS are
//! kept as thin as possible over them, because they can only ever be
//! type-checked here.

mod backend;
mod dpi;
mod enumerate;
mod ffi;
mod filter;
mod gdi;
mod geom;
mod pixels;
mod wgc;

pub use backend::backend;
