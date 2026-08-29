//! Serialised access to process environment variables, for tests only.
//!
//! # Why this exists
//!
//! `cargo test` runs tests in threads, and the environment is process-wide.
//! Several parts of the CLI read it deliberately — `SCROZZ_UNSTABLE_BACKENDS`
//! guards the unfinished backends, `WAYLAND_DISPLAY` decides whether window
//! enumeration is possible, `SCROZZ_IPC_SOCKET` moves the single-instance
//! endpoint — so the tests that cover those paths have to write it.
//!
//! Two threads doing that at once produce a failure that depends on scheduling,
//! which is the worst kind to debug: it passes locally and fails in CI, or the
//! other way round. Rust marks `set_var` `unsafe` for exactly this reason.
//!
//! So every test that touches the environment takes [`lock`] first, and the
//! guard restores what was there when it is dropped — including on a panic, so
//! one failing test cannot cascade into a dozen unrelated ones.

use std::sync::{Mutex, MutexGuard, OnceLock};

/// Every variable the test suite writes.
///
/// Snapshotting a fixed list rather than the whole environment keeps the guard
/// cheap and makes the blast radius of these tests visible in one place.
const MANAGED: &[&str] = &[
    "SCROZZ_UNSTABLE_BACKENDS",
    "SCROZZ_IPC_SOCKET",
    "SCROZZ_SIMULATE_ERROR",
    "SCROZZ_SETTINGS_FILE",
    "WAYLAND_DISPLAY",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "USER",
    "RUST_LOG",
];

fn mutex() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Holds the environment lock and restores the previous values on drop.
pub struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<String>)>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            // SAFETY: the lock is still held, so this is the only thread
            // touching the environment.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// Takes exclusive control of the environment for the rest of the scope.
///
/// A poisoned lock is recovered rather than propagated: the poison means some
/// other test panicked, and failing every subsequent test as well would bury the
/// one real failure.
#[must_use]
pub fn lock() -> EnvGuard {
    let guard = mutex()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let saved = MANAGED
        .iter()
        .map(|&key| (key, std::env::var(key).ok()))
        .collect();
    EnvGuard {
        _lock: guard,
        saved,
    }
}

/// Sets a variable. Call [`lock`] first.
pub fn set(key: &str, value: &str) {
    debug_assert!(
        MANAGED.contains(&key),
        "{key} is not in MANAGED, so it will not be restored"
    );
    // SAFETY: callers hold the guard returned by `lock`.
    unsafe { std::env::set_var(key, value) };
}

/// Removes a variable. Call [`lock`] first.
pub fn clear(key: &str) {
    debug_assert!(
        MANAGED.contains(&key),
        "{key} is not in MANAGED, so it will not be restored"
    );
    // SAFETY: callers hold the guard returned by `lock`.
    unsafe { std::env::remove_var(key) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_restores_what_was_there() {
        {
            let _env = lock();
            set("SCROZZ_UNSTABLE_BACKENDS", "1");
        }
        assert!(std::env::var("SCROZZ_UNSTABLE_BACKENDS").is_err());
    }

    #[test]
    fn the_guard_restores_a_pre_existing_value() {
        {
            let _env = lock();
            set("RUST_LOG", "outer");
        }
        // Re-take the lock, shadow it, and confirm the shadow is undone.
        {
            let _outer = lock();
            set("RUST_LOG", "outer");
            drop(_outer);
        }
        let _env = lock();
        set("RUST_LOG", "inner");
        assert_eq!(std::env::var("RUST_LOG").unwrap(), "inner");
    }

    #[test]
    fn a_panic_still_restores() {
        let before = std::env::var("WAYLAND_DISPLAY").ok();
        let result = std::panic::catch_unwind(|| {
            let _env = lock();
            set("WAYLAND_DISPLAY", "wayland-99");
            panic!("boom");
        });
        assert!(result.is_err());
        assert_eq!(std::env::var("WAYLAND_DISPLAY").ok(), before);
    }
}
