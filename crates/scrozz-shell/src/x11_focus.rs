//! Retained keyboard focus for X11 override-redirect selection windows.

use std::collections::HashSet;

use scrozz_core::{Error, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, Atom, AtomEnum, InputFocus, MapState, Window};
use x11rb::rust_connection::RustConnection;
use x11rb::{CURRENT_TIME, NONE};

const POINTER_ROOT: Window = 1;
const MAX_DISCOVERY_ATTEMPTS: u16 = 120;

/// Owns X11 keyboard focus for a native selection window and restores it on drop.
///
/// Window managers intentionally ignore override-redirect windows, so activation
/// requests cannot focus a selection overlay. This lease uses `SetInputFocus`
/// against the exact XID, keeps that focus while the picker is live, and restores
/// the window that was focused before selection began.
pub struct X11FocusLease {
    connection: RustConnection,
    previous_focus: FocusSnapshot,
    target: Option<Window>,
    discovery: Option<Discovery>,
    discovery_attempts: u16,
    restored: bool,
}

struct Discovery {
    roots: Vec<Window>,
    existing: HashSet<Window>,
    pid_atom: Atom,
}

#[derive(Debug, Clone, Copy)]
struct FocusSnapshot {
    window: Window,
    revert_to: InputFocus,
}

impl X11FocusLease {
    /// Snapshots the current focus before a picker window is created.
    ///
    /// Call [`Self::attach_window`] with the exact native XID from the newly
    /// created window, then call [`Self::maintain`] from its event loop. Taking
    /// the snapshot first guarantees restoration is not confused by a toolkit
    /// that focuses the window during creation.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the X server cannot be reached or queried.
    pub fn before_window() -> Result<Self> {
        let (connection, _) = connect()?;
        let previous_focus = input_focus(&connection)?;
        Ok(Self {
            connection,
            previous_focus,
            target: None,
            discovery: None,
            discovery_attempts: 0,
            restored: false,
        })
    }

    /// Attaches a pre-creation lease to the picker's exact native XID.
    ///
    /// # Errors
    ///
    /// Returns a platform error for an invalid XID, a restored lease, or a
    /// lease that is already attached.
    pub fn attach_window(&mut self, window: Window) -> Result<()> {
        if self.restored {
            return Err(Error::Platform(
                "cannot attach a restored X11 focus lease".to_owned(),
            ));
        }
        if self.target.is_some() || self.discovery.is_some() {
            return Err(Error::Platform(
                "cannot attach an X11 focus lease more than once".to_owned(),
            ));
        }
        validate_window(window)?;
        self.target = Some(window);
        Ok(())
    }

    /// Creates a lease for a known picker XID.
    ///
    /// The window may still be unmapped. Call [`Self::maintain`] from the event
    /// loop until it returns `true`, then continue calling it while selection is
    /// active so another client cannot accidentally retain the picker's input.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the X server cannot be reached or queried.
    pub fn for_window(window: Window) -> Result<Self> {
        let mut lease = Self::before_window()?;
        lease.attach_window(window)?;
        Ok(lease)
    }

    /// Creates a lease that attaches to the next override-redirect window made
    /// by this process.
    ///
    /// This is for immediate child viewports whose window handle is not exposed
    /// by eframe. The baseline is captured before asking eframe to create the
    /// viewport; [`Self::maintain`] then identifies the one newly mapped XID by
    /// creation identity, process id, and override-redirect state. No title,
    /// geometry, or external focus helper is involved.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the X server cannot be reached or queried.
    pub fn for_next_process_window() -> Result<Self> {
        let (connection, _) = connect()?;
        let roots = connection
            .setup()
            .roots
            .iter()
            .map(|screen| screen.root)
            .collect::<Vec<_>>();
        let existing = root_children(&connection, &roots)?;
        let pid_atom = xproto::intern_atom(&connection, false, b"_NET_WM_PID")
            .map_err(platform)?
            .reply()
            .map_err(platform)?
            .atom;
        let previous_focus = input_focus(&connection)?;
        Ok(Self {
            connection,
            previous_focus,
            target: None,
            discovery: Some(Discovery {
                roots,
                existing,
                pid_atom,
            }),
            discovery_attempts: 0,
            restored: false,
        })
    }

    /// Acquires or reasserts focus for the picker XID.
    ///
    /// Returns `false` while the native window is not mapped yet.
    ///
    /// # Errors
    ///
    /// Returns a platform error for X11 failures, an ambiguous child attachment,
    /// or when eframe never maps the expected child viewport.
    pub fn maintain(&mut self) -> Result<bool> {
        if self.restored {
            return Err(Error::Platform(
                "cannot reacquire a restored X11 focus lease".to_owned(),
            ));
        }

        let target = match self.target {
            Some(target) => target,
            None => {
                if self.discovery.is_none() {
                    return Err(Error::Platform(
                        "the X11 focus lease has no picker window attached".to_owned(),
                    ));
                }
                let Some(target) = self.discover_target()? else {
                    self.discovery_attempts = self.discovery_attempts.saturating_add(1);
                    if self.discovery_attempts >= MAX_DISCOVERY_ATTEMPTS {
                        return Err(Error::Platform(
                            "the X11 picker child viewport never became available for keyboard focus"
                                .to_owned(),
                        ));
                    }
                    return Ok(false);
                };
                self.target = Some(target);
                self.discovery = None;
                target
            }
        };

        let attributes = xproto::get_window_attributes(&self.connection, target)
            .map_err(platform)?
            .reply()
            .map_err(platform)?;
        if attributes.map_state != MapState::VIEWABLE {
            self.discovery_attempts = self.discovery_attempts.saturating_add(1);
            if self.discovery_attempts >= MAX_DISCOVERY_ATTEMPTS {
                return Err(Error::Platform(format!(
                    "X11 picker window {target:#x} never became viewable"
                )));
            }
            return Ok(false);
        }

        if input_focus(&self.connection)?.window != target {
            set_input_focus(&self.connection, target, InputFocus::PARENT)?;
        }
        Ok(input_focus(&self.connection)?.window == target)
    }

    /// Restores the focus that existed before the picker opened.
    ///
    /// Restoration is conditional: if the user or window manager has already
    /// moved focus elsewhere, the lease does not steal it back.
    ///
    /// # Errors
    ///
    /// Returns a platform error when X11 rejects the restoration request.
    pub fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }

        let Some(target) = self.target else {
            self.restored = true;
            return Ok(());
        };
        if input_focus(&self.connection)?.window != target {
            self.restored = true;
            return Ok(());
        }

        let destination = if self.previous_focus.window <= POINTER_ROOT
            || is_viewable(&self.connection, self.previous_focus.window)
        {
            self.previous_focus.window
        } else {
            POINTER_ROOT
        };
        set_input_focus(&self.connection, destination, self.previous_focus.revert_to)?;
        self.restored = true;
        Ok(())
    }

    /// The XID attached to this lease, once a deferred child has been discovered.
    #[must_use]
    pub const fn target(&self) -> Option<Window> {
        self.target
    }

    fn discover_target(&self) -> Result<Option<Window>> {
        let Some(discovery) = &self.discovery else {
            return Ok(self.target);
        };
        let children = root_children(&self.connection, &discovery.roots)?;
        let mut candidates = Vec::new();

        for window in children {
            if discovery.existing.contains(&window) {
                continue;
            }
            let Ok(cookie) = xproto::get_window_attributes(&self.connection, window) else {
                continue;
            };
            let Ok(attributes) = cookie.reply() else {
                continue;
            };
            if attributes.map_state != MapState::VIEWABLE || !attributes.override_redirect {
                continue;
            }
            if window_pid(&self.connection, window, discovery.pid_atom)? == Some(std::process::id())
            {
                candidates.push(window);
            }
        }

        unique_candidate(&candidates)
    }
}

impl Drop for X11FocusLease {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            tracing::warn!("could not restore X11 focus after selection: {error}");
        }
    }
}

fn connect() -> Result<(RustConnection, usize)> {
    x11rb::connect(None).map_err(|error| {
        Error::Platform(format!(
            "could not connect to X11 for picker focus: {error}"
        ))
    })
}

fn validate_window(window: Window) -> Result<()> {
    if window == NONE || window == POINTER_ROOT {
        return Err(Error::Platform(format!(
            "invalid X11 picker window id {window}"
        )));
    }
    Ok(())
}

fn root_children(connection: &RustConnection, roots: &[Window]) -> Result<HashSet<Window>> {
    let mut children = HashSet::new();
    for &root in roots {
        children.extend(
            xproto::query_tree(connection, root)
                .map_err(platform)?
                .reply()
                .map_err(platform)?
                .children,
        );
    }
    Ok(children)
}

fn input_focus(connection: &RustConnection) -> Result<FocusSnapshot> {
    let reply = xproto::get_input_focus(connection)
        .map_err(platform)?
        .reply()
        .map_err(platform)?;
    Ok(FocusSnapshot {
        window: reply.focus,
        revert_to: reply.revert_to,
    })
}

fn set_input_focus(
    connection: &RustConnection,
    window: Window,
    revert_to: InputFocus,
) -> Result<()> {
    xproto::set_input_focus(connection, revert_to, window, CURRENT_TIME)
        .map_err(platform)?
        .check()
        .map_err(platform)?;
    connection.flush().map_err(platform)
}

fn is_viewable(connection: &RustConnection, window: Window) -> bool {
    xproto::get_window_attributes(connection, window)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|attributes| attributes.map_state == MapState::VIEWABLE)
}

fn window_pid(connection: &RustConnection, window: Window, pid_atom: Atom) -> Result<Option<u32>> {
    let property = xproto::get_property(
        connection,
        false,
        window,
        pid_atom,
        u32::from(AtomEnum::CARDINAL),
        0,
        1,
    )
    .map_err(platform)?
    .reply()
    .map_err(platform)?;
    Ok(property.value32().and_then(|mut values| values.next()))
}

fn unique_candidate(candidates: &[Window]) -> Result<Option<Window>> {
    match candidates {
        [] => Ok(None),
        [window] => Ok(Some(*window)),
        _ => Err(Error::Platform(format!(
            "multiple new X11 override-redirect windows belong to this process: {}",
            candidates
                .iter()
                .map(|window| format!("{window:#x}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn platform(error: impl std::fmt::Display) -> Error {
    Error::Platform(format!("X11 picker focus failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_attachment_waits_for_one_candidate() {
        assert_eq!(unique_candidate(&[]).unwrap(), None);
        assert_eq!(unique_candidate(&[0x42]).unwrap(), Some(0x42));
    }

    #[test]
    fn child_attachment_refuses_ambiguous_candidates() {
        let error = unique_candidate(&[0x42, 0x43]).unwrap_err();
        assert!(error.to_string().contains("multiple new X11"));
    }

    #[test]
    fn focus_targets_must_be_real_windows() {
        assert!(validate_window(NONE).is_err());
        assert!(validate_window(POINTER_ROOT).is_err());
        assert!(validate_window(0x42).is_ok());
    }
}
