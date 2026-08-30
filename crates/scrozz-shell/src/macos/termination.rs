//! Keeps AppKit from retiring the menu-bar process while its root is hidden.

use objc2::rc::Retained;
use objc2_foundation::{NSProcessInfo, NSString};
use scrozz_core::Result;

use crate::macos::main_thread;

/// Stable reason emitted by AppKit's automatic-termination diagnostics.
pub const REASON: &str = "Scrozz application lifetime";

/// A balanced, application-lifetime automatic-termination inhibition.
///
/// AppKit may otherwise mark an accessory application with no visible windows
/// as automatically terminable. Scrozz intentionally spends most of its life
/// in exactly that state. AppKit balances its own "No windows open yet" lease
/// during root-window bootstrap, so the app-owned inhibition must be acquired
/// from the first eframe logic pass after the initial root frame has completed
/// and AppKit has settled that bootstrap lease. It is released from
/// `eframe::App::on_exit` after native adapters are retired.
pub struct AutomaticTerminationGuard {
    process: Retained<NSProcessInfo>,
    reason: Retained<NSString>,
    active: bool,
}

impl AutomaticTerminationGuard {
    /// Prevents AppKit automatic termination until this guard is released.
    ///
    /// # Errors
    ///
    /// Returns a platform error when called away from the application thread.
    pub fn acquire() -> Result<Self> {
        let _mtm = main_thread("disabling automatic termination")?;
        let process = NSProcessInfo::processInfo();
        let reason = NSString::from_str(REASON);
        process.disableAutomaticTermination(&reason);
        crate::macos::activity::record_automatic_termination_disable();
        tracing::info!(
            reason = REASON,
            support_enabled = process.automaticTerminationSupportEnabled(),
            "automatic termination inhibited after AppKit bootstrap"
        );
        Ok(Self {
            process,
            reason,
            active: true,
        })
    }

    /// Balances the inhibition exactly once.
    pub fn release(&mut self) {
        if !self.active {
            return;
        }
        self.process.enableAutomaticTermination(&self.reason);
        crate::macos::activity::record_automatic_termination_enable();
        tracing::info!(reason = REASON, "automatic termination inhibition released");
        self.active = false;
    }

    /// Whether this guard still owns its balancing enable call.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for AutomaticTerminationGuard {
    fn drop(&mut self) {
        self.release();
    }
}
