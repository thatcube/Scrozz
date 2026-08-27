//! Scrozz — screenshots and screen recording for macOS, Windows and Linux.
//!
//! # One binary, two front ends
//!
//! Per decision D11 the CLI is not a convenience wrapper bolted on later; it is
//! the architecture. Every capture the GUI can take, the CLI can take headlessly.
//!
//! That matters for three separate reasons:
//!
//! 1. **On wlroots compositors it is the only way hotkeys can work at all.**
//!    There is no global-shortcut portal there, so the user binds a compositor
//!    keybinding to `scrozz capture`. Without a CLI, Scrozz simply has no
//!    hotkeys on sway or Hyprland.
//! 2. **It makes the app scriptable**, which no competitor in this space does
//!    well.
//! 3. **It makes the app testable by agents**, who cannot click.

fn main() {
    todo!("parse arguments and dispatch to the GUI or a CLI subcommand")
}
