//! Ordinary macOS recording-editor window activation.

use objc2_app_kit::{
    NSApplication, NSColor, NSNormalWindowLevel, NSWindow, NSWindowCollectionBehavior,
};
use scrozz_core::{Error, Result};

use super::main_thread;

/// Native properties required of the recording editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorWindowDiagnostics {
    /// Whether AppKit considers this the active key window.
    pub key: bool,
    /// Whether AppKit considers this the application's main document window.
    pub main: bool,
    /// Whether it is ordered on screen.
    pub visible: bool,
    /// Whether it paints a solid background.
    pub opaque: bool,
    /// Whether mouse input reaches it.
    pub accepts_mouse: bool,
    /// Native stacking level.
    pub level: isize,
    /// Native collection behavior bits.
    pub collection_behavior: usize,
}

impl EditorWindowDiagnostics {
    /// Whether the observed window satisfies the ordinary-editor contract.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.key
            && self.main
            && self.visible
            && self.opaque
            && self.accepts_mouse
            && self.level == NSNormalWindowLevel
            && self.collection_behavior == NSWindowCollectionBehavior::Default.0
    }
}

/// Activates Scrozz and makes its titled editor the normal key/main window.
///
/// Returns `Ok(None)` while eframe has not created the deferred viewport yet,
/// allowing the main loop to retry on its next tick without treating ordinary
/// viewport creation latency as a failure.
///
/// # Errors
///
/// Returns [`Error::Platform`] off the main thread or when AppKit exposes a
/// matching window but refuses the required ordinary-window state.
#[allow(deprecated)]
pub fn activate(title: &str) -> Result<Option<EditorWindowDiagnostics>> {
    if title.trim().is_empty() {
        return Err(Error::InvalidRequest(
            "recording editor window title cannot be empty".to_owned(),
        ));
    }
    let mtm = main_thread("activating the recording editor")?;
    let application = NSApplication::sharedApplication(mtm);
    let windows = application.windows();
    let matching = (0..windows.count())
        .map(|index| {
            // SAFETY: the index comes from the immutable array's current range.
            unsafe { windows.objectAtIndex_unchecked(index) }
        })
        .find(|window| window.title().to_string() == title);
    let Some(window) = matching else {
        return Ok(None);
    };

    configure(window);
    application.activateIgnoringOtherApps(true);
    window.makeMainWindow();
    window.makeKeyAndOrderFront(None);
    let diagnostics = diagnostics(window);
    if !diagnostics.is_ready() {
        return Err(Error::Platform(format!(
            "recording editor did not become an ordinary interactive window: {diagnostics:?}"
        )));
    }
    Ok(Some(diagnostics))
}

fn configure(window: &NSWindow) {
    window.setLevel(NSNormalWindowLevel);
    window.setCollectionBehavior(NSWindowCollectionBehavior::Default);
    window.setIgnoresMouseEvents(false);
    window.setOpaque(true);
    window.setBackgroundColor(Some(&NSColor::windowBackgroundColor()));
    window.setHasShadow(true);
    window.setHidesOnDeactivate(false);
    window.setMovable(true);
    window.setMovableByWindowBackground(false);
}

fn diagnostics(window: &NSWindow) -> EditorWindowDiagnostics {
    EditorWindowDiagnostics {
        key: window.isKeyWindow(),
        main: window.isMainWindow(),
        visible: window.isVisible(),
        opaque: window.isOpaque(),
        accepts_mouse: !window.ignoresMouseEvents(),
        level: window.level(),
        collection_behavior: window.collectionBehavior().0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_reject_every_overlay_property() {
        let ordinary = EditorWindowDiagnostics {
            key: true,
            main: true,
            visible: true,
            opaque: true,
            accepts_mouse: true,
            level: NSNormalWindowLevel,
            collection_behavior: NSWindowCollectionBehavior::Default.0,
        };
        assert!(ordinary.is_ready());

        for broken in [
            EditorWindowDiagnostics {
                opaque: false,
                ..ordinary.clone()
            },
            EditorWindowDiagnostics {
                accepts_mouse: false,
                ..ordinary.clone()
            },
            EditorWindowDiagnostics {
                level: NSNormalWindowLevel + 1,
                ..ordinary.clone()
            },
            EditorWindowDiagnostics {
                key: false,
                ..ordinary.clone()
            },
            EditorWindowDiagnostics {
                main: false,
                ..ordinary
            },
        ] {
            assert!(!broken.is_ready());
        }
    }
}
