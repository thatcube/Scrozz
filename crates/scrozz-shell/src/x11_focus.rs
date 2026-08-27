//! Keyboard-focus ownership for an X11 override-redirect selection surface.
//!
//! Winit requests focus with `_NET_ACTIVE_WINDOW`. That message is handled by
//! the window manager, so it cannot affect an override-redirect window: the
//! window manager never sees that window. A selector must use `SetInputFocus`
//! directly and then restore the window that had focus before selection.

use scrozz_core::{Error, Result};
use x11rb::{
    connection::{Connection as _, RequestConnection as _},
    errors::ReplyError,
    protocol::xproto::{ConnectionExt as _, InputFocus, MapState, Window},
    rust_connection::RustConnection,
};

const POINTER_ROOT: Window = 1;

/// Owns keyboard focus while one known X11 selection window is visible.
///
/// The lease is intentionally tied to a numeric window ID obtained from the
/// application's native handle. It never searches by title and never asks an
/// external helper to guess which Scrozz window should receive input.
pub struct X11FocusLease {
    connection: RustConnection,
    window: Window,
    previous: Option<Window>,
    wants_focus: bool,
    owns_focus: bool,
}

impl std::fmt::Debug for X11FocusLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("X11FocusLease")
            .field("window", &self.window)
            .field("wants_focus", &self.wants_focus)
            .field("owns_focus", &self.owns_focus)
            .finish_non_exhaustive()
    }
}

impl X11FocusLease {
    /// Attaches to an existing override-redirect window.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when no X server is available or the
    /// window is managed by the window manager, and [`Error::TargetGone`] when
    /// the supplied window no longer exists.
    pub fn adopt(window: u32) -> Result<Self> {
        let (connection, _) = x11rb::connect(None).map_err(|error| Error::Unsupported {
            what: "X11 selector keyboard focus".to_owned(),
            why: format!("could not connect to the X server: {error}"),
        })?;
        let attributes = connection
            .get_window_attributes(window)
            .map_err(|error| platform("reading selector window attributes", error))?
            .reply()
            .map_err(|error| window_reply_error(window, "reading selector attributes", error))?;
        if !attributes.override_redirect {
            return Err(Error::Unsupported {
                what: "direct X11 selector keyboard focus".to_owned(),
                why: "the selector window is managed; its window manager owns focus requests"
                    .to_owned(),
            });
        }

        Ok(Self {
            connection,
            window,
            previous: None,
            wants_focus: false,
            owns_focus: false,
        })
    }

    /// Changes whether this selection surface should own the keyboard.
    ///
    /// Acquisition is deferred while the window is not viewable. Call
    /// [`Self::refresh`] from the UI loop after requesting that the window be
    /// shown.
    ///
    /// # Errors
    ///
    /// Returns a platform error when X11 rejects or fails to apply the focus
    /// transition.
    pub fn set_wants_focus(&mut self, wants_focus: bool) -> Result<()> {
        self.wants_focus = wants_focus;
        if wants_focus {
            let _ = self.acquire_if_viewable()?;
            Ok(())
        } else {
            self.release()
        }
    }

    /// Retries a pending acquisition after native window commands are applied.
    ///
    /// # Errors
    ///
    /// Returns a platform error when X11 rejects or fails to apply focus.
    pub fn refresh(&mut self) -> Result<()> {
        if self.wants_focus {
            let _ = self.acquire_if_viewable()?;
        }
        Ok(())
    }

    fn acquire_if_viewable(&mut self) -> Result<bool> {
        if self.owns_focus {
            return Ok(true);
        }

        let attributes = self
            .connection
            .get_window_attributes(self.window)
            .map_err(|error| platform("reading selector visibility", error))?
            .reply()
            .map_err(|error| {
                window_reply_error(self.window, "reading selector visibility", error)
            })?;
        if attributes.map_state != MapState::VIEWABLE {
            return Ok(false);
        }

        let current = self.current_focus()?;
        if current == self.window {
            return Ok(true);
        }

        self.connection
            .set_input_focus(InputFocus::POINTER_ROOT, self.window, x11rb::CURRENT_TIME)
            .map_err(|error| platform("requesting selector keyboard focus", error))?
            .check()
            .map_err(|error| platform("applying selector keyboard focus", error))?;
        self.connection
            .flush()
            .map_err(|error| platform("flushing selector keyboard focus", error))?;

        if self.current_focus()? != self.window {
            return Err(Error::Platform(format!(
                "X11 did not give keyboard focus to selector window {}",
                self.window
            )));
        }

        self.previous = Some(current);
        self.owns_focus = true;
        Ok(true)
    }

    fn release(&mut self) -> Result<()> {
        if !self.owns_focus {
            self.previous = None;
            return Ok(());
        }

        if self.current_focus()? != self.window {
            // The user or window manager moved focus elsewhere while selection
            // was open. Restoring the older window now would steal it back.
            self.previous = None;
            self.owns_focus = false;
            return Ok(());
        }

        let previous = self.previous.unwrap_or(POINTER_ROOT);
        let (target, vanished) = self.restore_target(previous)?;
        self.connection
            .set_input_focus(InputFocus::POINTER_ROOT, target, x11rb::CURRENT_TIME)
            .map_err(|error| platform("restoring X11 keyboard focus", error))?
            .check()
            .map_err(|error| platform("applying restored X11 keyboard focus", error))?;
        self.connection
            .flush()
            .map_err(|error| platform("flushing restored X11 keyboard focus", error))?;
        self.previous = None;
        self.owns_focus = false;

        if let Some(error) = vanished {
            Err(Error::TargetGone(format!(
                "the X11 window that owned keyboard focus before selection ({previous}) disappeared; \
                 focus now follows the pointer ({error})"
            )))
        } else {
            Ok(())
        }
    }

    fn restore_target(&self, previous: Window) -> Result<(Window, Option<String>)> {
        if previous <= POINTER_ROOT {
            return Ok((previous, None));
        }

        let attributes = self
            .connection
            .get_window_attributes(previous)
            .map_err(|error| platform("requesting prior focus window attributes", error))?
            .reply();
        match attributes {
            Ok(attributes) if attributes.map_state == MapState::VIEWABLE => Ok((previous, None)),
            Ok(_) => Ok((
                POINTER_ROOT,
                Some("the prior focus window is no longer viewable".to_owned()),
            )),
            Err(ReplyError::X11Error(error)) => Ok((
                POINTER_ROOT,
                Some(format!("the X server rejected the prior window: {error:?}")),
            )),
            Err(ReplyError::ConnectionError(error)) => {
                Err(platform("reading prior focus window attributes", error))
            }
        }
    }

    fn current_focus(&self) -> Result<Window> {
        self.connection
            .get_input_focus()
            .map_err(|error| platform("reading X11 keyboard focus", error))?
            .reply()
            .map(|reply| reply.focus)
            .map_err(|error| platform("reading X11 keyboard focus reply", error))
    }
}

impl Drop for X11FocusLease {
    fn drop(&mut self) {
        self.wants_focus = false;
        if let Err(error) = self.release() {
            tracing::warn!(%error, "could not release X11 selector keyboard focus");
        }
    }
}

fn platform(context: &str, error: impl std::fmt::Display) -> Error {
    Error::Platform(format!("{context}: {error}"))
}

fn window_reply_error(window: Window, context: &str, error: ReplyError) -> Error {
    match error {
        ReplyError::X11Error(error) => {
            Error::TargetGone(format!("X11 window {window}: {context}: {error:?}"))
        }
        ReplyError::ConnectionError(error) => platform(context, error),
    }
}
