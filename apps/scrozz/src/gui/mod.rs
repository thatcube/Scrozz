//! The menu-bar application: the wiring between a keypress and a card.
//!
//! # What this module is
//!
//! Every part of the pipeline already exists somewhere else. `scrozz-capture`
//! takes the screenshot, `scrozz-store` keeps it, `scrozz-export` encodes and
//! copies it, `scrozz-shell` owns the tray and the hotkeys, `scrozz-ui` paints
//! the card. None of them know about each other. This module is the only place
//! that does.
//!
//! ```text
//!    hotkey ─┐
//!      tray ─┼──▶ Action ──▶ Pipeline ──▶ Card ──▶ CardSurface
//!       IPC ─┘   (main)      (worker)              (main)
//!                                │                    │
//!                          store + export        CardEvent
//!                                                     │
//!                                            copy / save / dismiss
//! ```
//!
//! # The main-thread problem
//!
//! This is the constraint the whole design is shaped around, and it fails
//! *silently* when got wrong, which is why it is written down here rather than
//! discovered later.
//!
//! - `tray-icon` and `muda` are built on `Rc`, so their types are `!Send` and
//!   cannot cross a thread boundary at all.
//! - `GlobalHotKeyManager` needs the main thread **with a platform event loop
//!   already running on it**: macOS delivers Carbon hotkey events to the main
//!   run loop, Windows to that thread's message queue. Nothing reports it if
//!   there is no loop — the hotkey simply never fires.
//! - winit, and therefore eframe, wants the main thread too.
//!
//! Three things, one thread. So [`App`] is not a loop: it is a state machine
//! with a single [`App::tick`] that services all three sources once and
//! returns. Whichever host owns the main loop calls `tick` from its own
//! per-iteration callback — an eframe `update`, or [`host::Headless`]'s sleep
//! loop. Anything that blocks (reading the screen, encoding a PNG, writing to
//! SQLite) happens on the [`pipeline`] worker instead.
//!
//! # Never `set_event_handler`
//!
//! Both `global-hotkey` and `muda` expose a `set_event_handler`, and both store
//! it in a process-global `OnceCell`. The first caller wins and every other
//! consumer in the process — including the next version of this file — is
//! starved with no diagnostic. `scrozz-shell` exposes `poll`/`drain` over the
//! receiver instead, and [`App::tick`] uses those.
//!
//! # What is not here yet
//!
//! The one thing this module cannot supply for itself is a *window*. Creating
//! one means winit or AppKit, and `apps/scrozz` depends on neither: it is the
//! binary a compositor keybinding invokes, and linking a windowing stack into
//! it is exactly backwards. The window belongs to `scrozz-ui`, which already
//! has eframe.
//!
//! So the seam is [`card::CardSurface`], a five-method trait this module owns.
//! [`crate::platform::card_surface`] returns the real one when `scrozz-ui`
//! exposes it, and a recording stand-in otherwise, which is what makes the rest
//! of the pipeline testable — and runnable — today.

pub mod action;
pub mod app;
pub mod card;
pub mod host;
pub mod overlay;
pub mod panel;
pub mod pipeline;
pub mod server;
pub mod settings_window;

// Re-exported so the rest of the binary — and anything that later lifts this
// module into a library — names these from one place. A binary crate has no
// external consumers, so the compiler cannot see the uses and warns; that is a
// property of the crate type, not of the code.
#[allow(unused_imports)]
pub use crate::gui::{
    action::{Action, CaptureKind},
    app::{App, Config},
    card::{Card, CardEvent, CardId, CardSurface, Thumbnail},
    host::Host,
    overlay::OverlayCards,
};

use crate::{cli::Cli, fault::CliResult, report::Report};

/// Launches the menu-bar app, and returns when it quits.
///
/// # Ordering
///
/// The activation policy is set **first**, before anything that could put
/// pixels on screen. D27 says Scrozz is invisible at rest; a Dock icon that
/// appears for a few hundred milliseconds during startup and then vanishes is
/// still a Dock icon the user saw.
///
/// # Errors
///
/// Returns [`crate::fault::CliError::NotImplemented`] when no host owns a main
/// loop on this platform — see the module docs. Any other failure is a real
/// one: a tray that could not be created, a socket that could not be bound.
pub fn run(cli: &Cli) -> CliResult<Report> {
    crate::platform::become_accessory_app()?;

    let config = Config::from_cli(cli)?;

    // Handed to the host because a windowed run cannot always return to `main`
    // to have its report printed — see `host::Driver::logic`.
    let reporter = crate::report::Reporter::from_global(&cli.global);
    let emit: host::Emit = Box::new(move |report: &Report| {
        if let Err(e) = reporter.emit("gui", report) {
            tracing::warn!("the report could not be written: {e}");
        }
    });

    let host = host::for_platform(&config, emit)?;
    let app = App::new(config, host.surface())?;
    host.run(app)
}

/// Whether a run of `scrozz gui` will put a card on screen.
///
/// Distinct from "the GUI runs": with `SCROZZ_GUI_HEADLESS=1` everything except
/// the card works — hotkeys, the menu-bar item, capture, the store, the
/// clipboard. Reporting that as availability would be a lie of exactly the kind
/// this module is built to avoid, because the one thing the user asked to see
/// is the one thing that would not appear.
#[must_use]
pub fn available() -> bool {
    host::HAS_WINDOW
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_means_a_card_appears_not_merely_that_it_runs() {
        // If this ever returns true while `host::for_platform` still refuses,
        // the two have drifted and `scrozz --json` is reporting a capability
        // that does not exist.
        if available() {
            assert!(
                host::for_platform(&Config::sealed(), Box::new(|_| {})).is_ok(),
                "availability claims a window this build cannot open"
            );
        }
    }

    #[test]
    fn the_headless_switch_is_named_consistently() {
        assert_eq!(host::HEADLESS_ENV, "SCROZZ_GUI_HEADLESS");
    }
}
