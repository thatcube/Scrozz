//! Platform custom-colour picker.
//!
//! The editor owns its compact quick palette. This module is only the escape
//! hatch behind “Custom colour”: AppKit's shared colour panel where available,
//! and an explicit unavailable result everywhere else so `scrozz-ui` can open
//! its capable egui fallback.

use scrozz_core::Result;

/// Something the platform colour picker reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPickerEvent {
    /// The picker changed its sRGBA value.
    Changed([u8; 4]),
    /// The picker closed, carrying its final sRGBA value and whether that value
    /// changed since the last delivered event.
    Closed {
        /// Final straight-alpha sRGBA value.
        color: [u8; 4],
        /// Whether closure delivered a color the caller has not seen.
        changed: bool,
    },
}

/// Main-thread handle to the platform's custom-colour picker.
#[derive(Debug, Default)]
pub struct SystemColorPicker {
    active: bool,
    last: [u8; 4],
}

impl SystemColorPicker {
    /// Opens the platform picker at `color`.
    ///
    /// Returns `false` when this platform has no system picker integration, so
    /// the caller can open its cross-platform fallback.
    pub fn open(&mut self, color: [u8; 4]) -> Result<bool> {
        #[cfg(target_os = "macos")]
        {
            macos::open(color)?;
            self.active = true;
            self.last = color;
            Ok(true)
        }

        #[cfg(not(target_os = "macos"))]
        {
            self.last = color;
            Ok(false)
        }
    }

    /// Polls one change without blocking.
    pub fn poll(&mut self) -> Result<Option<ColorPickerEvent>> {
        if !self.active {
            return Ok(None);
        }

        #[cfg(target_os = "macos")]
        {
            match macos::poll()? {
                Some(ColorPickerEvent::Changed(color)) if color != self.last => {
                    self.last = color;
                    Ok(Some(ColorPickerEvent::Changed(color)))
                }
                Some(ColorPickerEvent::Closed { color, .. }) => {
                    let changed = color != self.last;
                    self.active = false;
                    self.last = color;
                    Ok(Some(ColorPickerEvent::Closed { color, changed }))
                }
                _ => Ok(None),
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            self.active = false;
            Ok(None)
        }
    }

    /// Closes the picker if it is showing.
    pub fn close(&mut self) -> Result<()> {
        #[cfg(target_os = "macos")]
        if self.active {
            macos::close()?;
        }
        self.active = false;
        Ok(())
    }

    /// Whether the platform picker is currently being polled.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.active
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2_app_kit::{NSApplication, NSColor, NSColorPanel, NSColorSpace};

    use super::ColorPickerEvent;
    use scrozz_core::{Error, Result};

    pub fn open(color: [u8; 4]) -> Result<()> {
        let mtm = crate::macos::main_thread("opening the system colour picker")?;
        let panel = NSColorPanel::sharedColorPanel(mtm);
        panel.setShowsAlpha(true);
        panel.setContinuous(true);
        panel.setColor(&NSColor::colorWithSRGBRed_green_blue_alpha(
            component(color[0]),
            component(color[1]),
            component(color[2]),
            component(color[3]),
        ));
        let app = NSApplication::sharedApplication(mtm);
        app.activate();
        // SAFETY: `nil` is the documented sender for programmatic ordering.
        unsafe { app.orderFrontColorPanel(None) };
        Ok(())
    }

    pub fn poll() -> Result<Option<ColorPickerEvent>> {
        let mtm = crate::macos::main_thread("reading the system colour picker")?;
        let panel = NSColorPanel::sharedColorPanel(mtm);
        let color = panel_color(&panel)?;
        if !panel.isVisible() {
            let app = NSApplication::sharedApplication(mtm);
            return Ok(panel_visibility_event(false, app.isActive(), color));
        }

        Ok(Some(ColorPickerEvent::Changed(color)))
    }

    fn panel_color(panel: &NSColorPanel) -> Result<[u8; 4]> {
        let srgb = panel
            .color()
            .colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())
            .ok_or_else(|| {
                Error::Platform("the system colour picker returned a non-RGB colour".to_owned())
            })?;
        Ok([
            channel(srgb.redComponent()),
            channel(srgb.greenComponent()),
            channel(srgb.blueComponent()),
            channel(srgb.alphaComponent()),
        ])
    }

    pub fn close() -> Result<()> {
        let mtm = crate::macos::main_thread("closing the system colour picker")?;
        if NSColorPanel::sharedColorPanelExists(mtm) {
            NSColorPanel::sharedColorPanel(mtm).orderOut(None);
        }
        Ok(())
    }

    fn component(channel: u8) -> f64 {
        f64::from(channel) / 255.0
    }

    fn channel(component: f64) -> u8 {
        (component.clamp(0.0, 1.0) * 255.0).round() as u8
    }

    fn panel_visibility_event(
        visible: bool,
        application_active: bool,
        color: [u8; 4],
    ) -> Option<ColorPickerEvent> {
        (!visible && application_active).then_some(ColorPickerEvent::Closed {
            color,
            changed: false,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn appkit_components_round_trip_to_bytes() {
            for value in [0, 1, 127, 128, 254, 255] {
                assert_eq!(channel(component(value)), value);
            }
        }

        #[test]
        fn application_deactivation_does_not_close_picker_tracking() {
            let color = [12, 34, 56, 78];
            assert_eq!(panel_visibility_event(false, false, color), None);
            assert_eq!(
                panel_visibility_event(false, true, color),
                Some(ColorPickerEvent::Closed {
                    color,
                    changed: false
                })
            );
            assert_eq!(panel_visibility_event(true, true, color), None);
        }
    }
}
