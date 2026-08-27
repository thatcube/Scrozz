//! X11 scroll synthesis through the XTEST extension.

use scrozz_core::{Error, Result, ScrollCapabilities, ScrollDriver, ScrollGesture};
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
        let delivery = (0..notches).try_for_each(|_| {
            self.fake_input(xproto::BUTTON_PRESS_EVENT, button, x, y)?;
            self.fake_input(xproto::BUTTON_RELEASE_EVENT, button, x, y)
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
