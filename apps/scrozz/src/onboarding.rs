//! The four-topic getting-started flow from decision D26.
//!
//! Permissions are intentionally absent. Scrozz asks for an invasive capability
//! only when the user first invokes the feature that needs it; onboarding teaches
//! the interactions that would otherwise be invisible.

use crate::{cli::HotkeyAction, fault::CliResult, hotkey_config, settings_store::SettingsStore};
use scrozz_core::Error as CoreError;

/// The current getting-started content version.
pub const CURRENT_VERSION: u32 = 1;

/// One topic in the fixed D26 sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    /// A capture card can be dragged directly into another app.
    DragOut,
    /// The primary capture shortcut.
    CaptureHotkey,
    /// The capture output folder.
    CaptureFolder,
    /// The compositor-owned shortcut required on wlroots.
    CompositorKeybinding,
}

impl Topic {
    /// Every topic, in presentation order.
    pub const ALL: [Self; 4] = [
        Self::DragOut,
        Self::CaptureHotkey,
        Self::CaptureFolder,
        Self::CompositorKeybinding,
    ];

    /// The concise heading.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::DragOut => "Drag captures anywhere",
            Self::CaptureHotkey => "Capture without breaking focus",
            Self::CaptureFolder => "Know where captures go",
            Self::CompositorKeybinding => "Connect your compositor shortcut",
        }
    }

    /// The explanatory copy.
    #[must_use]
    pub const fn body(self) -> &'static str {
        match self {
            Self::DragOut => {
                "Drag a capture card into a message, document, folder, or any app that accepts files."
            }
            Self::CaptureHotkey => {
                "Use the region shortcut from any app. Scrozz returns you to what you were doing after the capture."
            }
            Self::CaptureFolder => {
                "Every capture is saved to your chosen folder unless you provide another destination."
            }
            Self::CompositorKeybinding => {
                "Some Wayland compositors keep global shortcuts in their own configuration. Paste the generated line there once."
            }
        }
    }
}

/// Progress through the getting-started sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flow {
    index: usize,
    visible: bool,
}

impl Flow {
    /// Builds the startup state from persisted completion.
    ///
    /// # Errors
    ///
    /// Returns a settings error if the persisted values cannot be read.
    pub fn from_store(store: &SettingsStore) -> CliResult<Self> {
        let completed = store.get("onboarding.completed")?.1 == "true";
        let raw_version = store.get("onboarding.version")?.1;
        let version = raw_version.parse::<u32>().map_err(|error| {
            crate::fault::CliError::Core(CoreError::Storage(format!(
                "validated onboarding.version {raw_version:?} is unreadable: {error}"
            )))
        })?;
        Ok(Self {
            index: 0,
            visible: !completed || version < CURRENT_VERSION,
        })
    }

    /// Starts the flow even when it was completed before.
    pub fn rerun(&mut self) {
        self.index = 0;
        self.visible = true;
    }

    /// Whether the flow should be drawn.
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        self.visible
    }

    /// The current topic.
    #[must_use]
    pub fn topic(&self) -> Topic {
        Topic::ALL[self.index]
    }

    /// The zero-based position.
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    /// Moves back when possible.
    pub fn back(&mut self) {
        self.index = self.index.saturating_sub(1);
    }

    /// Moves forward, persisting completion after the final topic.
    ///
    /// # Errors
    ///
    /// Returns a storage error if final completion cannot be persisted.
    pub fn next(&mut self, store: &mut SettingsStore) -> CliResult<()> {
        if self.index + 1 < Topic::ALL.len() {
            self.index += 1;
            return Ok(());
        }
        self.complete(store)
    }

    /// Skips the remaining topics and records this version as seen.
    ///
    /// # Errors
    ///
    /// Returns a storage error if completion cannot be persisted.
    pub fn skip(&mut self, store: &mut SettingsStore) -> CliResult<()> {
        self.complete(store)
    }

    fn complete(&mut self, store: &mut SettingsStore) -> CliResult<()> {
        mark_complete(store)?;
        self.visible = false;
        Ok(())
    }
}

/// Records the current onboarding content version as seen.
///
/// Used by the native settings window, whose visual state machine lives in
/// `scrozz-ui` while persistence remains in this app crate.
pub fn mark_complete(store: &mut SettingsStore) -> CliResult<()> {
    store.apply(&[
        ("onboarding.completed".to_owned(), "true".to_owned()),
        ("onboarding.version".to_owned(), CURRENT_VERSION.to_string()),
    ])
}

/// The compositor binding line for the current session, when one is needed.
///
/// # Errors
///
/// Returns a validation error if a persisted accelerator is no longer usable by
/// the compositor generator.
pub fn compositor_config(store: &SettingsStore) -> CliResult<Option<String>> {
    let Some(compositor) = hotkey_config::detect_compositor() else {
        return Ok(None);
    };
    let accelerator = store.get("hotkey.capture-region")?.1;
    hotkey_config::generate(
        compositor,
        "scrozz",
        Some(HotkeyAction::CaptureRegion),
        Some(accelerator),
    )
    .map(|generated| {
        generated
            .bindings
            .into_iter()
            .next()
            .map(|binding| binding.line)
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    fn store(name: &str) -> (PathBuf, SettingsStore) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "scrozz-onboarding-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("settings.json");
        let store = SettingsStore::open(path).unwrap();
        (directory, store)
    }

    #[test]
    fn the_flow_has_exactly_the_four_d26_topics() {
        assert_eq!(
            Topic::ALL,
            [
                Topic::DragOut,
                Topic::CaptureHotkey,
                Topic::CaptureFolder,
                Topic::CompositorKeybinding,
            ]
        );
    }

    #[test]
    fn a_new_profile_is_visible_and_can_move_both_directions() {
        let (directory, store) = store("navigation");
        let mut flow = Flow::from_store(&store).unwrap();
        assert!(flow.is_visible());
        assert_eq!(flow.topic(), Topic::DragOut);
        flow.next(&mut SettingsStore::open(store.path()).unwrap())
            .unwrap();
        assert_eq!(flow.topic(), Topic::CaptureHotkey);
        flow.back();
        assert_eq!(flow.topic(), Topic::DragOut);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn skip_is_persistent_and_rerun_is_explicit() {
        let (directory, mut store) = store("skip");
        let mut flow = Flow::from_store(&store).unwrap();
        flow.skip(&mut store).unwrap();
        assert!(!flow.is_visible());

        let reloaded = SettingsStore::open(store.path()).unwrap();
        assert!(!Flow::from_store(&reloaded).unwrap().is_visible());
        flow.rerun();
        assert!(flow.is_visible());
        assert_eq!(flow.index(), 0);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn finishing_the_last_topic_persists_the_content_version() {
        let (directory, mut store) = store("finish");
        let mut flow = Flow::from_store(&store).unwrap();
        for _ in 0..Topic::ALL.len() {
            flow.next(&mut store).unwrap();
        }
        assert!(!flow.is_visible());
        assert_eq!(
            store.get("onboarding.version").unwrap().1,
            CURRENT_VERSION.to_string()
        );
        let _ = fs::remove_dir_all(directory);
    }
}
