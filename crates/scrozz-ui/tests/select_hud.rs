//! Selector-focused regression tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scrozz_core::selection::{SelectionCapabilities, SelectionMode};
use scrozz_ui::{HudModel, HudNav};

#[test]
fn hud_entries_use_selection_mode_labels_and_descriptions() {
    let hud = HudModel::new(SelectionMode::Region, SelectionCapabilities::CLIENT_OVERLAY);
    let entries = hud.entries();

    assert_eq!(entries[0].label, SelectionMode::Region.label());
    assert_eq!(entries[1].description, SelectionMode::Window.description());
    assert!(
        entries
            .iter()
            .all(|entry| !entry.label.is_empty() && !entry.description.is_empty())
    );
}

#[test]
fn navigation_skips_disabled_modes() {
    let mut hud = HudModel::new(
        SelectionMode::Region,
        SelectionCapabilities {
            window_picking: false,
            ..SelectionCapabilities::CLIENT_OVERLAY
        },
    );

    hud.navigate(HudNav::Next);
    assert_eq!(hud.activate_focused().unwrap(), SelectionMode::Display);
}

#[test]
fn unsupported_selection_cannot_be_selected() {
    let mut hud = HudModel::new(
        SelectionMode::Region,
        SelectionCapabilities {
            window_picking: false,
            ..SelectionCapabilities::CLIENT_OVERLAY
        },
    );

    assert!(!hud.select(SelectionMode::Window));
    assert_eq!(hud.selected(), SelectionMode::Region);
}
