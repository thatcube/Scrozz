//! Lifetime-scoped native input monitoring for recording interactions.

#[cfg(target_os = "macos")]
#[path = "macos/input.rs"]
mod platform;

#[cfg(target_os = "macos")]
pub(crate) use platform::active_count;
#[cfg(target_os = "macos")]
pub(crate) use platform::start;
