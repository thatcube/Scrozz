//! X11 scroll synthesis through the XTEST extension.

use scrozz_core::{Error, Result, ScrollCapabilities, ScrollDriver, ScrollGesture, WindowId};
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{xproto, xtest},
    rust_connection::RustConnection,
};

use crate::scroll_units;

/// XTEST-backed scroll synthesis.
pub(crate) struct X11ScrollDriver {
    conn: RustConnection,
    root: u32,
    scale: scrozz_core::ScaleFactor,
    prepared: bool,
}

impl std::fmt::Debug for X11ScrollDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11ScrollDriver")
            .field("root", &self.root)
            .field("scale", &self.scale.get())
            .field("prepared", &self.prepared)
            .finish_non_exhaustive()
    }
}

impl X11ScrollDriver {
    /// Connects and verifies that the server advertises a usable XTEST version.
    pub(crate) fn connect() -> Result<Self> {
        let (conn, screen_index) = x11rb::connect(None)
            .map_err(|err| Error::Platform(format!("could not connect to the X server: {err}")))?;
        let root = conn
            .setup()
            .roots
            .get(screen_index)
            .ok_or_else(|| Error::Platform("the X server reported no screens".into()))?
            .root;
        validate_xtest(&conn)?;

        let scale = super::read_scale(&conn, root);
        Ok(Self {
            conn,
            root,
            scale,
            prepared: false,
        })
    }

    fn fake_input(&self, type_: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        xtest::fake_input(&self.conn, type_, detail, 0, self.root, x, y, 0)
            .map_err(platform)?
            .check()
            .map_err(platform)
    }

    fn move_pointer(&self, x: i16, y: i16) -> Result<()> {
        self.fake_input(xproto::MOTION_NOTIFY_EVENT, 0, x, y)
    }

    fn root_child_for(&self, mut window: u32) -> Result<u32> {
        for _ in 0..64 {
            let tree = xproto::query_tree(&self.conn, window)
                .map_err(platform)?
                .reply()
                .map_err(platform)?;
            if tree.parent == self.root {
                return Ok(window);
            }
            if tree.parent == 0 || tree.parent == window {
                return Err(Error::TargetGone(
                    "the selected X11 window is no longer attached to this screen".into(),
                ));
            }
            window = tree.parent;
        }
        Err(Error::Platform(
            "the selected X11 target has an unexpectedly deep window hierarchy".into(),
        ))
    }

    fn ensure_selected_window_owns_pointer(
        &self,
        gesture: &ScrollGesture,
        x: i16,
        y: i16,
    ) -> Result<()> {
        let id = gesture.window.as_ref().ok_or_else(|| Error::Unsupported {
            what: "automatic scrolling of an unspecified X11 target".into(),
            why: "XTEST wheel input is pointer-addressed, so Scrozz requires the exact selected \
                  window before it can post one safely"
                .into(),
        })?;
        let selected = parse_window_id(id)?;
        let attributes = xproto::get_window_attributes(&self.conn, selected)
            .map_err(platform)?
            .reply()
            .map_err(platform)?;
        if attributes.map_state != xproto::MapState::VIEWABLE {
            return Err(Error::TargetGone(format!(
                "X11 window {} is no longer viewable",
                id.0
            )));
        }
        let geometry = xproto::get_geometry(&self.conn, selected)
            .map_err(platform)?
            .reply()
            .map_err(platform)?;
        let relative = xproto::query_pointer(&self.conn, selected)
            .map_err(platform)?
            .reply()
            .map_err(platform)?;
        let inside = relative.same_screen
            && relative.win_x >= 0
            && relative.win_y >= 0
            && i32::from(relative.win_x) < i32::from(geometry.width)
            && i32::from(relative.win_y) < i32::from(geometry.height);
        if !inside {
            return Err(Error::TargetGone(format!(
                "X11 window {} no longer contains the selected scroll point",
                id.0
            )));
        }

        let selected_root_child = self.root_child_for(selected)?;
        let root_pointer = xproto::query_pointer(&self.conn, self.root)
            .map_err(platform)?
            .reply()
            .map_err(platform)?;
        if root_pointer.root_x != x
            || root_pointer.root_y != y
            || root_pointer.child != selected_root_child
        {
            return Err(Error::TargetGone(format!(
                "X11 window {} no longer owns the selected scroll point",
                id.0
            )));
        }
        Ok(())
    }
}

impl ScrollDriver for X11ScrollDriver {
    fn capabilities(&self) -> ScrollCapabilities {
        ScrollCapabilities::automatic(false)
    }

    fn prepare(&mut self) -> Result<()> {
        if self.prepared {
            return Ok(());
        }
        // Revalidate at the permission boundary rather than relying only on the
        // factory probe; the driver must not claim success after a server reset.
        validate_xtest(&self.conn)?;
        self.prepared = true;
        Ok(())
    }

    fn scroll(&mut self, gesture: &ScrollGesture) -> Result<()> {
        if gesture.is_noop() {
            return Ok(());
        }
        if !self.prepared {
            return Err(Error::Platform(
                "the XTEST scroll driver must be prepared before it can send input".into(),
            ));
        }
        if !scroll_units::finite_point(gesture.at) {
            return Err(Error::InvalidRequest(
                "the scroll target point must contain finite coordinates".into(),
            ));
        }

        let x = i16::try_from((gesture.at.x * self.scale.get()).round() as i64).map_err(|_| {
            Error::InvalidRequest("the X11 scroll target x coordinate is out of range".into())
        })?;
        let y = i16::try_from((gesture.at.y * self.scale.get()).round() as i64).map_err(|_| {
            Error::InvalidRequest("the X11 scroll target y coordinate is out of range".into())
        })?;
        let original = xproto::query_pointer(&self.conn, self.root)
            .map_err(platform)?
            .reply()
            .map_err(platform)?;

        self.move_pointer(x, y)?;
        let (button, notches) = scroll_units::x11_button_and_notches(gesture.axis, gesture.amount);
        let delivery = self
            .ensure_selected_window_owns_pointer(gesture, x, y)
            .and_then(|()| {
                (0..notches).try_for_each(|_| {
                    self.fake_input(xproto::BUTTON_PRESS_EVENT, button, x, y)?;
                    self.fake_input(xproto::BUTTON_RELEASE_EVENT, button, x, y)
                })
            });
        let restore = self.move_pointer(original.root_x, original.root_y);

        match (delivery, restore) {
            (Err(delivery), _) => Err(delivery),
            (Ok(()), Err(restore)) => Err(Error::Platform(format!(
                "XTEST delivered the wheel input but could not restore the pointer: {restore}"
            ))),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    fn name(&self) -> &str {
        "XTEST"
    }
}

fn validate_xtest(conn: &RustConnection) -> Result<()> {
    let present = conn
        .extension_information(xtest::X11_EXTENSION_NAME)
        .map_err(platform)?
        .is_some();
    if !present {
        return Err(Error::Unsupported {
            what: "automatic scrolling through X11".into(),
            why: "this X server does not advertise the XTEST extension; enable XTEST in the \
                  server, or scroll manually while Scrozz follows"
                .into(),
        });
    }

    let version = xtest::get_version(conn, 2, 2)
        .map_err(platform)?
        .reply()
        .map_err(platform)?;
    if (version.major_version, version.minor_version) < (2, 1) {
        return Err(Error::Unsupported {
            what: "automatic scrolling through X11".into(),
            why: format!(
                "the X server implements XTEST {}.{}, but fake pointer input requires XTEST 2.1 \
                 or newer; update the X server, or scroll manually while Scrozz follows",
                version.major_version, version.minor_version
            ),
        });
    }
    Ok(())
}

fn platform(error: impl std::fmt::Display) -> Error {
    Error::Platform(format!("XTEST request failed: {error}"))
}

fn parse_window_id(id: &WindowId) -> Result<u32> {
    id.0.strip_prefix("x11:")
        .and_then(|raw| u32::from_str_radix(raw, 16).ok())
        .ok_or_else(|| {
            Error::InvalidRequest(format!("window id {} is not an X11 window identity", id.0))
        })
}

#[cfg(test)]
mod tests {
    use super::parse_window_id;
    use scrozz_core::WindowId;

    #[test]
    fn target_identity_parser_accepts_only_x11_handles() {
        assert_eq!(
            parse_window_id(&WindowId("x11:00abcdef".to_owned())).expect("X11 id"),
            0x00ab_cdef
        );
        assert!(parse_window_id(&WindowId("42".to_owned())).is_err());
        assert!(parse_window_id(&WindowId("x11:not-hex".to_owned())).is_err());
    }
}
