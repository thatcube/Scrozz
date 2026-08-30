//! Best-effort ICCCM/EWMH behavior for movable X11 pin windows.

use std::{cell::RefCell, rc::Rc};

use scrozz_core::{Error, LogicalRect, Result, ScaleFactor};
use x11rb::{
    connection::Connection,
    protocol::xproto::{
        Atom, AtomEnum, ClientMessageData, ClientMessageEvent, ConfigureWindowAux, ConnectionExt,
        EventMask, PropMode, Window,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use crate::{OverlayBehavior, OverlayReport, OverlayWindow};

const ICCCM_INPUT_HINT: u32 = 1;
const NET_WM_STATE_ADD: u32 = 1;
const NET_WM_SOURCE_APPLICATION: u32 = 1;
/// `_NET_WM_USER_TIME` for a window no user interaction asked for.
const NO_USER_INTERACTION_TIME: u32 = 0;

#[derive(Debug)]
struct Atoms {
    utf8_string: Atom,
    net_client_list: Atom,
    net_wm_name: Atom,
    net_wm_pid: Atom,
    net_wm_window_type: Atom,
    net_wm_window_type_dock: Atom,
    net_wm_state: Atom,
    net_wm_state_above: Atom,
    net_wm_state_sticky: Atom,
    net_wm_state_skip_taskbar: Atom,
    net_wm_state_skip_pager: Atom,
    net_wm_user_time: Atom,
    net_wm_user_time_window: Atom,
    wm_hints: Atom,
    wm_protocols: Atom,
    wm_take_focus: Atom,
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Self> {
        let names: [&[u8]; 16] = [
            b"UTF8_STRING",
            b"_NET_CLIENT_LIST",
            b"_NET_WM_NAME",
            b"_NET_WM_PID",
            b"_NET_WM_WINDOW_TYPE",
            b"_NET_WM_WINDOW_TYPE_DOCK",
            b"_NET_WM_STATE",
            b"_NET_WM_STATE_ABOVE",
            b"_NET_WM_STATE_STICKY",
            b"_NET_WM_STATE_SKIP_TASKBAR",
            b"_NET_WM_STATE_SKIP_PAGER",
            b"_NET_WM_USER_TIME",
            b"_NET_WM_USER_TIME_WINDOW",
            b"WM_HINTS",
            b"WM_PROTOCOLS",
            b"WM_TAKE_FOCUS",
        ];
        let cookies = names
            .into_iter()
            .map(|name| conn.intern_atom(false, name).map_err(platform))
            .collect::<Result<Vec<_>>>()?;
        let atoms = cookies
            .into_iter()
            .map(|cookie| cookie.reply().map(|reply| reply.atom).map_err(platform))
            .collect::<Result<Vec<_>>>()?;
        let [
            utf8_string,
            net_client_list,
            net_wm_name,
            net_wm_pid,
            net_wm_window_type,
            net_wm_window_type_dock,
            net_wm_state,
            net_wm_state_above,
            net_wm_state_sticky,
            net_wm_state_skip_taskbar,
            net_wm_state_skip_pager,
            net_wm_user_time,
            net_wm_user_time_window,
            wm_hints,
            wm_protocols,
            wm_take_focus,
        ] = atoms.as_slice()
        else {
            return Err(Error::Platform(
                "the X11 pin atom table was incomplete".into(),
            ));
        };
        Ok(Self {
            utf8_string: *utf8_string,
            net_client_list: *net_client_list,
            net_wm_name: *net_wm_name,
            net_wm_pid: *net_wm_pid,
            net_wm_window_type: *net_wm_window_type,
            net_wm_window_type_dock: *net_wm_window_type_dock,
            net_wm_state: *net_wm_state,
            net_wm_state_above: *net_wm_state_above,
            net_wm_state_sticky: *net_wm_state_sticky,
            net_wm_state_skip_taskbar: *net_wm_state_skip_taskbar,
            net_wm_state_skip_pager: *net_wm_state_skip_pager,
            net_wm_user_time: *net_wm_user_time,
            net_wm_user_time_window: *net_wm_user_time_window,
            wm_hints: *wm_hints,
            wm_protocols: *wm_protocols,
            wm_take_focus: *wm_take_focus,
        })
    }
}

#[derive(Debug)]
struct X11Context {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
}

thread_local! {
    static X11_CONTEXT: RefCell<Option<Rc<X11Context>>> = const { RefCell::new(None) };
}

fn context() -> Result<Rc<X11Context>> {
    X11_CONTEXT.with(|slot| {
        if let Some(context) = slot.borrow().as_ref() {
            return Ok(Rc::clone(context));
        }
        let (conn, screen) = RustConnection::connect(None).map_err(|error| Error::Unsupported {
            what: "native X11 pinned-window adaptation".into(),
            why: format!("no X11 connection is available: {error}"),
        })?;
        let root = conn
            .setup()
            .roots
            .get(screen)
            .ok_or_else(|| Error::Platform("the X11 screen index is invalid".into()))?
            .root;
        let atoms = Atoms::intern(&conn)?;
        let context = Rc::new(X11Context { conn, root, atoms });
        *slot.borrow_mut() = Some(Rc::clone(&context));
        Ok(context)
    })
}

/// A process-owned X11 client window with best-effort non-focus hints.
#[derive(Debug)]
pub struct X11Overlay {
    context: Rc<X11Context>,
    window: Window,
    title: String,
}

impl X11Overlay {
    /// Finds exactly one own-process client carrying Scrozz's pin app id.
    pub fn find_by_title(title: &str) -> Result<Option<Self>> {
        let context = context()?;
        let conn = &context.conn;
        let atoms = &context.atoms;
        let clients = property32(
            conn,
            context.root,
            atoms.net_client_list,
            AtomEnum::WINDOW.into(),
        )?;
        let process = std::process::id();
        let mut matches = Vec::new();
        for window in clients {
            let Ok(pid) = property32(conn, window, atoms.net_wm_pid, AtomEnum::CARDINAL.into())
            else {
                continue;
            };
            if pid.first().copied() != Some(process) {
                continue;
            }
            let Ok(window_name) = property8(conn, window, atoms.net_wm_name, atoms.utf8_string)
            else {
                continue;
            };
            if window_name != title.as_bytes() {
                continue;
            }
            matches.push(window);
        }

        match matches.as_slice() {
            [] => Ok(None),
            [window] => Ok(Some(Self {
                context,
                window: *window,
                title: title.to_owned(),
            })),
            matches => Err(Error::Platform(format!(
                "refusing ambiguous X11 pin title {title:?}: {} own-process clients matched",
                matches.len()
            ))),
        }
    }

    /// Applies the strongest portable EWMH/ICCCM hints without claiming a guarantee.
    pub fn apply(&mut self, _behavior: &OverlayBehavior) -> Result<OverlayReport> {
        self.validate()?;
        let conn = &self.context.conn;
        let atoms = &self.context.atoms;
        conn.change_property32(
            PropMode::REPLACE,
            self.window,
            atoms.net_wm_window_type,
            AtomEnum::ATOM,
            &[atoms.net_wm_window_type_dock],
        )
        .map_err(platform)?;

        let mut hints =
            property32(conn, self.window, atoms.wm_hints, atoms.wm_hints).unwrap_or_default();
        hints.resize(9, 0);
        hints[0] |= ICCCM_INPUT_HINT;
        hints[1] = 0;
        conn.change_property32(
            PropMode::REPLACE,
            self.window,
            atoms.wm_hints,
            atoms.wm_hints,
            &hints,
        )
        .map_err(platform)?;

        // EWMH's own answer to "do not focus this window". A zero user time is
        // defined to mean the window was not created by user interaction, and a
        // conforming manager must not focus it on map or on request. It is the
        // one non-activation instruction in the specification that does not
        // depend on override-redirect, which movable pins cannot use because it
        // takes move and resize away from the window manager.
        //
        // A client may redirect the manager to read the time from a separate
        // window, and toolkits do: writing to the toplevel while the manager is
        // reading `_NET_WM_USER_TIME_WINDOW` would set a value nothing consults
        // and look, from here, exactly like success.
        let time_window = property32(
            conn,
            self.window,
            atoms.net_wm_user_time_window,
            AtomEnum::WINDOW.into(),
        )
        .ok()
        .and_then(|windows| windows.first().copied())
        .filter(|window| *window != 0)
        .unwrap_or(self.window);
        conn.change_property32(
            PropMode::REPLACE,
            time_window,
            atoms.net_wm_user_time,
            AtomEnum::CARDINAL,
            &[NO_USER_INTERACTION_TIME],
        )
        .map_err(platform)?;

        let protocols = property32(conn, self.window, atoms.wm_protocols, AtomEnum::ATOM.into())?;
        let protocols = without_take_focus(protocols, atoms.wm_take_focus);
        conn.change_property32(
            PropMode::REPLACE,
            self.window,
            atoms.wm_protocols,
            AtomEnum::ATOM,
            &protocols,
        )
        .map_err(platform)?;

        for state in [
            atoms.net_wm_state_above,
            atoms.net_wm_state_sticky,
            atoms.net_wm_state_skip_taskbar,
            atoms.net_wm_state_skip_pager,
        ] {
            let event = ClientMessageEvent::new(
                32,
                self.window,
                atoms.net_wm_state,
                ClientMessageData::from([NET_WM_STATE_ADD, state, 0, NET_WM_SOURCE_APPLICATION, 0]),
            );
            conn.send_event(
                false,
                self.context.root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )
            .map_err(platform)?;
        }

        conn.flush().map_err(platform)?;

        Ok(OverlayReport {
            non_activating: false,
            detail: "X11 ICCCM input=false, no WM_TAKE_FOCUS, _NET_WM_USER_TIME=0 and EWMH dock/sticky/above hints are window-manager policy, not a focus guarantee".into(),
        })
    }

    /// X11 properties disappear with the client window; no class retrofit is retained.
    pub fn restore_native_class(&mut self) -> Result<()> {
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        let conn = &self.context.conn;
        let atoms = &self.context.atoms;
        let pid = property32(
            conn,
            self.window,
            atoms.net_wm_pid,
            AtomEnum::CARDINAL.into(),
        )?;
        let title = property8(conn, self.window, atoms.net_wm_name, atoms.utf8_string)?;
        if pid.first().copied() != Some(std::process::id()) || title != self.title.as_bytes() {
            return Err(Error::TargetGone(format!(
                "X11 pinned window {:?} no longer identifies this process client",
                self.title
            )));
        }
        Ok(())
    }
}

impl OverlayWindow for X11Overlay {
    fn set_frame(&mut self, frame: LogicalRect) -> Result<()> {
        self.set_frame_with_scale(frame, ScaleFactor::IDENTITY)
    }

    fn set_frame_with_scale(&mut self, frame: LogicalRect, scale: ScaleFactor) -> Result<()> {
        self.validate()?;
        let scale = scale.get();
        let x = checked_i32(frame.origin.x * scale, "x")?;
        let y = checked_i32(frame.origin.y * scale, "y")?;
        let width = checked_u32(frame.size.width * scale, "width")?;
        let height = checked_u32(frame.size.height * scale, "height")?;
        self.context
            .conn
            .configure_window(
                self.window,
                &ConfigureWindowAux::new()
                    .x(x)
                    .y(y)
                    .width(width)
                    .height(height),
            )
            .map_err(platform)?;
        self.context.conn.flush().map_err(platform)
    }

    fn set_click_through(&mut self, _passthrough: bool) -> Result<()> {
        Err(Error::Unsupported {
            what: "X11 click-through pinning".into(),
            why: "pointer transparency without a focus-release contract is intentionally disabled"
                .into(),
        })
    }
}

fn property8(
    conn: &RustConnection,
    window: Window,
    property: Atom,
    property_type: Atom,
) -> Result<Vec<u8>> {
    conn.get_property(false, window, property, property_type, 0, u32::MAX)
        .map_err(platform)?
        .reply()
        .map(|reply| reply.value)
        .map_err(platform)
}

fn without_take_focus(protocols: Vec<Atom>, take_focus: Atom) -> Vec<Atom> {
    protocols
        .into_iter()
        .filter(|atom| *atom != take_focus)
        .collect()
}

fn property32(
    conn: &RustConnection,
    window: Window,
    property: Atom,
    property_type: Atom,
) -> Result<Vec<u32>> {
    let reply = conn
        .get_property(false, window, property, property_type, 0, u32::MAX)
        .map_err(platform)?
        .reply()
        .map_err(platform)?;
    Ok(reply.value32().map_or_else(Vec::new, Iterator::collect))
}

fn checked_i32(value: f64, label: &str) -> Result<i32> {
    if !value.is_finite() || value < f64::from(i32::MIN) || value > f64::from(i32::MAX) {
        return Err(Error::InvalidRequest(format!(
            "X11 pin {label} is outside the root-window range"
        )));
    }
    Ok(value.round() as i32)
}

fn checked_u32(value: f64, label: &str) -> Result<u32> {
    if !value.is_finite() || value < 1.0 || value > f64::from(u16::MAX) {
        return Err(Error::InvalidRequest(format!(
            "X11 pin {label} is outside the core protocol range"
        )));
    }
    Ok(value.round() as u32)
}

fn platform(error: impl std::fmt::Display) -> Error {
    Error::Platform(format!("X11 pin adapter failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_protocol_is_removed_without_dropping_delete_or_ping() {
        assert_eq!(without_take_focus(vec![10, 20, 30], 20), vec![10, 30]);
    }

    #[test]
    fn x11_core_geometry_is_checked_before_wire_conversion() {
        assert!(checked_i32(f64::NAN, "x").is_err());
        assert!(checked_u32(0.0, "width").is_err());
        assert!(checked_u32(f64::from(u16::MAX) + 1.0, "width").is_err());
        assert_eq!(checked_u32(4096.0, "width").unwrap(), 4096);
    }
}
