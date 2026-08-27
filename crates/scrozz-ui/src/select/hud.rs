use scrozz_core::selection::{SelectionCapabilities, SelectionMode};

/// Keyboard movement within the All-in-One HUD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HudNav {
    /// Move to the previous mode.
    Previous,
    /// Move to the next mode.
    Next,
}

/// One HUD entry derived from [`SelectionMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HudEntry {
    /// The mode this entry switches to.
    pub mode: SelectionMode,
    /// Stable user-facing label.
    pub label: &'static str,
    /// Stable accessibility description.
    pub description: &'static str,
    /// Whether this mode is currently available.
    pub enabled: bool,
    /// Whether this entry is the active mode.
    pub selected: bool,
    /// Whether keyboard focus is on this entry.
    pub focused: bool,
}

/// Pure state for the All-in-One mode picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HudModel {
    selected: SelectionMode,
    focused: usize,
    capabilities: SelectionCapabilities,
}

impl HudModel {
    /// Creates a HUD model for `selected` and the measured capabilities.
    #[must_use]
    pub fn new(selected: SelectionMode, capabilities: SelectionCapabilities) -> Self {
        let mut me = Self {
            selected,
            focused: 0,
            capabilities,
        };
        me.focus_selected_or_first_enabled();
        me
    }

    /// Every HUD entry in stable mode order.
    #[must_use]
    pub fn entries(&self) -> Vec<HudEntry> {
        SelectionMode::ALL
            .into_iter()
            .enumerate()
            .map(|(index, mode)| HudEntry {
                mode,
                label: mode.label(),
                description: mode.description(),
                enabled: self.capabilities.supports(mode),
                selected: self.selected == mode,
                focused: self.focused == index,
            })
            .collect()
    }

    /// The currently selected mode.
    #[must_use]
    pub const fn selected(&self) -> SelectionMode {
        self.selected
    }

    /// Selects `mode` when it is supported.
    #[must_use]
    pub fn select(&mut self, mode: SelectionMode) -> bool {
        if !self.capabilities.supports(mode) {
            return false;
        }
        if self.selected == mode {
            return true;
        }
        self.selected = mode;
        self.focus_selected_or_first_enabled();
        true
    }

    /// Moves keyboard focus within the enabled entries.
    pub fn navigate(&mut self, nav: HudNav) {
        let enabled: Vec<usize> = self
            .entries()
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.enabled.then_some(index))
            .collect();
        if enabled.is_empty() {
            return;
        }
        let current = enabled
            .iter()
            .position(|index| *index == self.focused)
            .unwrap_or(0);
        let next = match nav {
            HudNav::Previous => (current + enabled.len() - 1) % enabled.len(),
            HudNav::Next => (current + 1) % enabled.len(),
        };
        self.focused = enabled[next];
    }

    /// Activates the focused entry if it is enabled.
    #[must_use]
    pub fn activate_focused(&mut self) -> Option<SelectionMode> {
        let mode = SelectionMode::ALL[self.focused];
        self.select(mode).then_some(mode)
    }

    /// Replaces the capability set.
    pub fn set_capabilities(&mut self, capabilities: SelectionCapabilities) {
        if self.capabilities == capabilities {
            return;
        }
        self.capabilities = capabilities;
        if !self.capabilities.supports(self.selected) {
            self.selected = first_supported(capabilities).unwrap_or(SelectionMode::Region);
        }
        let focus_is_enabled = SelectionMode::ALL
            .get(self.focused)
            .is_some_and(|mode| capabilities.supports(*mode));
        if !focus_is_enabled {
            self.focus_selected_or_first_enabled();
        }
    }

    fn focus_selected_or_first_enabled(&mut self) {
        self.focused = SelectionMode::ALL
            .into_iter()
            .position(|mode| mode == self.selected && self.capabilities.supports(mode))
            .or_else(|| {
                SelectionMode::ALL
                    .into_iter()
                    .position(|mode| self.capabilities.supports(mode))
            })
            .unwrap_or(0);
    }
}

fn first_supported(capabilities: SelectionCapabilities) -> Option<SelectionMode> {
    SelectionMode::ALL
        .into_iter()
        .find(|mode| capabilities.supports(*mode))
}
