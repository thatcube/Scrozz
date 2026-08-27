//! The X11 overlay backend.
//!
//! X11 is the one Linux display server that does everything Scrozz needs: a
//! client may place a window at an absolute position, raise it above everything
//! else, keep it off the taskbar, and cut holes in its input region so clicks
//! fall through the empty space around the capture cards. This module is where
//! that happens.
//!
//! It opens its **own** connection to `$DISPLAY` rather than borrowing winit's.
//! `raw-window-handle` hands out a window *ID* — a 32-bit integer — not a
//! pointer to anyone's connection, and sharing an `xcb_connection_t` across two
//! libraries means matching their threading assumptions exactly. A second
//! connection costs one socket and removes the entire class of problem; X11
//! window IDs are server-side and valid from any connection.
//!
//! The decisions this module acts on live in [`super::ewmh`] and
//! [`super::region`], where they can be tested without an X server. What is left
//! here is the part that genuinely needs one.

use scrozz_core::{Error, LogicalRect, Result};
use x11rb::connection::{Connection as _, RequestConnection as _};
use x11rb::protocol::shape::{self, ConnectionExt as _, SK, SO};
use x11rb::protocol::xproto::{
    self, AtomEnum, ClientMessageEvent, ClipOrdering, ConfigureWindowAux, ConnectionExt as _,
    EventMask, InputFocus, PropMode, Rectangle, StackMode, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::ewmh::{self, Managed, WireRect, X11Plan};
use super::region::{InputRegion, RegionRect};
use crate::overlay::{OverlayBehavior, OverlayReport};

/// A connection to the X server plus the atoms Scrozz uses.
///
/// Atoms are interned once, in one batch, because each is a round trip and the
/// overlay path runs while the user is waiting.
pub struct X11Backend {
    conn: RustConnection,
    screen: usize,
    atoms: Atoms,
    has_shape: bool,
}

/// Interned atom values, keyed by the names in [`ewmh::required_atoms`].
struct Atoms {
    names: Vec<(&'static str, xproto::Atom)>,
}

impl Atoms {
    /// Looks an atom up by the name it was interned under.
    ///
    /// Returns `None` for a name that was never interned, which is a
    /// programming error rather than a runtime condition — hence the caller
    /// treats it as "skip this property" instead of failing the whole apply.
    fn get(&self, name: &str) -> Option<xproto::Atom> {
        self.names
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, atom)| *atom)
    }
}

impl X11Backend {
    /// Connects to `$DISPLAY` and interns Scrozz's atoms.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when there is no X server to connect to,
    /// because that is a fact about the session rather than a failure: a Wayland
    /// session without XWayland genuinely has none.
    pub fn connect() -> Result<Self> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| Error::Unsupported {
            what: "X11 overlay backend".into(),
            why: format!("could not connect to the X server: {e}"),
        })?;

        // SHAPE is what makes click-through possible. Its absence is not fatal
        // — the overlay still anchors and stacks — but it must be reported, not
        // discovered later as "clicks do not reach the window behind".
        let has_shape = conn
            .extension_information(shape::X11_EXTENSION_NAME)
            .map(|info| info.is_some())
            .unwrap_or(false);

        let names = ewmh::required_atoms();
        let cookies: Vec<_> = names
            .iter()
            .map(|name| conn.intern_atom(false, name.as_bytes()))
            .collect();
        let mut interned = Vec::with_capacity(names.len());
        for (name, cookie) in names.into_iter().zip(cookies) {
            let atom = cookie
                .map_err(platform)?
                .reply()
                .map_err(|e| platform(format!("interning {name}: {e}")))?
                .atom;
            interned.push((name, atom));
        }

        Ok(Self {
            conn,
            screen,
            atoms: Atoms { names: interned },
            has_shape,
        })
    }

    /// Whether the server offers the SHAPE extension, and therefore
    /// click-through.
    #[must_use]
    pub const fn supports_input_shaping(&self) -> bool {
        self.has_shape
    }

    /// The root window of the screen this connection is on.
    fn root(&self) -> Window {
        self.conn.setup().roots[self.screen].root
    }

    /// Reads a window's `override_redirect` attribute.
    ///
    /// Asked rather than assumed. winit's override-redirect setting is a
    /// creation-time request that can be ignored, and every property this module
    /// sets afterwards is either meaningful or dead depending on the answer.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TargetGone`] if the window has been destroyed.
    pub fn managed_state(&self, window: Window) -> Result<Managed> {
        let attrs = self
            .conn
            .get_window_attributes(window)
            .map_err(platform)?
            .reply()
            .map_err(|e| Error::TargetGone(format!("X11 window {window}: {e}")))?;
        Ok(if attrs.override_redirect {
            Managed::OverrideRedirect
        } else {
            Managed::ByWindowManager
        })
    }

    /// Applies overlay behaviour to a window and reports what actually happened.
    ///
    /// The report is the contract: it says which of the requested properties
    /// were applied and which were skipped, and it never claims a property was
    /// set on an override-redirect window, where nothing would have read it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if the X server rejected a request, and
    /// [`Error::TargetGone`] if the window no longer exists.
    pub fn apply(&self, window: Window, behavior: &OverlayBehavior) -> Result<OverlayReport> {
        let managed = self.managed_state(window)?;
        let plan = ewmh::plan_for(behavior, managed);
        let mut done: Vec<String> = Vec::new();

        if !plan.states.is_empty() {
            self.add_states(window, &plan)?;
            let listed: Vec<&str> = plan.states.iter().map(|s| s.atom_name()).collect();
            done.push(format!("set {}", listed.join(", ")));
        }

        if plan.all_desktops
            && let Some(atom) = self.atoms.get("_NET_WM_DESKTOP")
        {
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    window,
                    atom,
                    AtomEnum::CARDINAL,
                    &[ewmh::ALL_DESKTOPS],
                )
                .map_err(platform)?;
            done.push("pinned to all desktops".into());
        }

        // The ICCCM way to say "do not give me the keyboard". Only meaningful
        // for a managed window; an override-redirect window is never offered
        // focus in the first place, so there is nothing to decline.
        if managed == Managed::ByWindowManager {
            self.set_focusable(window, plan.takes_focus)?;
            done.push(if plan.takes_focus {
                "WM_HINTS.input = True".into()
            } else {
                "WM_HINTS.input = False (will not take focus)".into()
            });
        }

        if plan.client_restacks {
            self.conn
                .configure_window(
                    window,
                    &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                )
                .map_err(platform)?;
            done.push("raised".into());
        }

        if plan.client_focuses {
            self.conn
                .set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)
                .map_err(platform)?;
            done.push("took keyboard focus directly".into());
        }

        self.conn.flush().map_err(platform)?;

        Ok(OverlayReport {
            // On X11 "non-activating" means the window does not take focus when
            // clicked. A managed window achieves it by clearing WM_HINTS.input;
            // an override-redirect window achieves it by construction, since no
            // window manager will ever hand it focus.
            non_activating: !plan.takes_focus,
            detail: describe(&plan, &done, self.has_shape),
        })
    }

    /// Sends `_NET_WM_STATE` add messages to the root window.
    ///
    /// A client message to the root, not a property write: once a window is
    /// mapped, EWMH requires the state be changed by asking the window manager,
    /// because it owns the property from that point on. Writing it directly is
    /// the classic bug that appears to work until the window manager next
    /// rewrites the property.
    fn add_states(&self, window: Window, plan: &X11Plan) -> Result<()> {
        let Some(state_atom) = self.atoms.get("_NET_WM_STATE") else {
            return Ok(());
        };
        let root = self.root();
        for state in &plan.states {
            let Some(atom) = self.atoms.get(state.atom_name()) else {
                continue;
            };
            let event = ClientMessageEvent::new(
                32,
                window,
                state_atom,
                // action, first property, second property, source indication
                // (1 = a normal application), unused.
                [ewmh::state_action::ADD, atom, 0, 1, 0],
            );
            self.conn
                .send_event(
                    false,
                    root,
                    EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                    event,
                )
                .map_err(platform)?;
        }
        Ok(())
    }

    /// Sets or clears the `input` flag in `WM_HINTS`, preserving the rest.
    fn set_focusable(&self, window: Window, focusable: bool) -> Result<()> {
        let Some(atom) = self.atoms.get("WM_HINTS") else {
            return Ok(());
        };
        let existing = self
            .conn
            .get_property(
                false,
                window,
                atom,
                atom,
                0,
                u32::try_from(ewmh::WM_HINTS_WORDS).unwrap_or(9),
            )
            .map_err(platform)?
            .reply()
            .ok()
            .map(|reply| reply.value);

        let bytes = ewmh::encode_wm_hints_input(existing.as_deref(), focusable);
        self.conn
            .change_property(
                PropMode::REPLACE,
                window,
                atom,
                atom,
                32,
                u32::try_from(ewmh::WM_HINTS_WORDS).unwrap_or(9),
                &bytes,
            )
            .map_err(platform)?;
        Ok(())
    }

    /// Moves and resizes a window in root coordinates.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for a frame that does not fit X11's
    /// 16-bit coordinate space, and [`Error::Platform`] if the server refused.
    pub fn set_frame(&self, window: Window, frame: LogicalRect, scale: f64) -> Result<()> {
        let x = to_i32(frame.origin.x * scale)?;
        let y = to_i32(frame.origin.y * scale)?;
        let width = to_u32(frame.size.width * scale)?;
        let height = to_u32(frame.size.height * scale)?;

        self.conn
            .configure_window(
                window,
                &ConfigureWindowAux::new()
                    .x(x)
                    .y(y)
                    .width(width)
                    .height(height)
                    .stack_mode(StackMode::ABOVE),
            )
            .map_err(platform)?;
        self.conn.flush().map_err(platform)?;
        Ok(())
    }

    /// Replaces a window's input region, making the uncovered parts
    /// click-through.
    ///
    /// This is what delivers D27's invisibility at rest: the capture stack is a
    /// full-height window that is empty except for a few cards, and without an
    /// input region the empty space swallows every click in that column.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] when the server has no SHAPE extension —
    /// never silence, because the visible symptom is a dead strip down the side
    /// of the screen and the cause is not guessable from it.
    pub fn set_input_region(&self, window: Window, region: &InputRegion) -> Result<()> {
        if !self.has_shape {
            return Err(Error::Unsupported {
                what: "click-through overlay".into(),
                why: "this X server does not offer the SHAPE extension, so the overlay \
                      intercepts clicks across its whole rectangle"
                    .into(),
            });
        }

        let rects: Vec<Rectangle> = match region {
            // "Everything" is the absence of a shape, and removing the shape is
            // not the same as setting it to one big rectangle: an unshaped
            // window follows its own geometry as it is resized.
            InputRegion::Everything => {
                self.conn
                    .shape_mask(SO::SET, SK::INPUT, window, 0, 0, x11rb::NONE)
                    .map_err(platform)?
                    .check()
                    .map_err(platform)?;
                self.conn.flush().map_err(platform)?;
                return Ok(());
            }
            InputRegion::Nothing => Vec::new(),
            InputRegion::Rects(rects) => rects.iter().map(to_x11_rect).collect(),
        };

        self.conn
            .shape_rectangles(
                SO::SET,
                SK::INPUT,
                ClipOrdering::UNSORTED,
                window,
                0,
                0,
                &rects,
            )
            .map_err(platform)?
            .check()
            .map_err(platform)?;
        self.conn.flush().map_err(platform)?;
        Ok(())
    }

    /// Reads the current desktop's work area from `_NET_WORKAREA`.
    ///
    /// Returns `None` when the window manager publishes no work area, so the
    /// caller can fall back to full screen bounds rather than anchor to zero.
    #[must_use]
    pub fn work_area(&self) -> Option<WireRect> {
        let root = self.root();
        let desktop = self
            .read_cardinals(root, "_NET_CURRENT_DESKTOP")
            .and_then(|bytes| {
                bytes
                    .as_chunks::<4>()
                    .0
                    .first()
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            })
            .unwrap_or(0);
        let bytes = self.read_cardinals(root, "_NET_WORKAREA")?;
        ewmh::parse_work_area(&bytes, desktop)
    }

    /// The full bounds of the screen, as a last resort when `_NET_WORKAREA` is
    /// absent.
    #[must_use]
    pub fn screen_bounds(&self) -> WireRect {
        let screen = &self.conn.setup().roots[self.screen];
        WireRect {
            x: 0,
            y: 0,
            width: u32::from(screen.width_in_pixels),
            height: u32::from(screen.height_in_pixels),
        }
    }

    /// Reads a `CARDINAL[]` property, returning its raw bytes.
    fn read_cardinals(&self, window: Window, name: &str) -> Option<Vec<u8>> {
        let atom = self.atoms.get(name)?;
        let reply = self
            .conn
            .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        if reply.value.is_empty() {
            None
        } else {
            Some(reply.value)
        }
    }
}

/// Turns a plan and a list of completed steps into one sentence for a human.
fn describe(plan: &X11Plan, done: &[String], has_shape: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("X11 ({})", plan.window_type.atom_name()));
    if done.is_empty() {
        parts.push("nothing to apply".into());
    } else {
        parts.push(done.join("; "));
    }
    if !has_shape {
        parts.push("no SHAPE extension, so clicks cannot pass through".into());
    }
    for note in &plan.notes {
        parts.push((*note).to_string());
    }
    parts.join(" — ")
}

/// Clamps a Scrozz rectangle into X11's 16-bit coordinate space.
///
/// X11 has carried 16-bit window coordinates since 1987 and is not going to
/// change. Clamping rather than failing is right here: an input region that has
/// been clipped at the edge of a very large virtual screen is still usable,
/// while an error would take down the whole overlay for a rectangle the user
/// cannot see anyway.
fn to_x11_rect(rect: &RegionRect) -> Rectangle {
    Rectangle {
        x: rect.x.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
        y: rect.y.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
        width: rect.width.min(u32::from(u16::MAX)) as u16,
        height: rect.height.min(u32::from(u16::MAX)) as u16,
    }
}

/// Rounds a logical coordinate to a pixel, refusing values X11 cannot express.
fn to_i32(value: f64) -> Result<i32> {
    if !value.is_finite() {
        return Err(Error::InvalidRequest(format!(
            "overlay frame coordinate {value} is not a finite number"
        )));
    }
    Ok(value
        .round()
        .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i32)
}

/// Rounds a logical extent to a pixel, refusing values X11 cannot express.
fn to_u32(value: f64) -> Result<u32> {
    if !value.is_finite() || value < 0.0 {
        return Err(Error::InvalidRequest(format!(
            "overlay frame extent {value} is not a usable size"
        )));
    }
    Ok(value.round().min(f64::from(u16::MAX)) as u32)
}

/// Wraps an X11 failure as a platform error.
fn platform(error: impl std::fmt::Display) -> Error {
    Error::Platform(format!("X11: {error}"))
}
