//! What Scrozz asks an X11 window manager for, and what it must do itself.
//!
//! X11 has two completely different windows wearing the same name, and the
//! difference decides which half of this file applies.
//!
//! A **managed** window is one the window manager reparents and controls.
//! Everything in EWMH is available to it: `_NET_WM_STATE_ABOVE` keeps it on top,
//! `_NET_WM_STATE_SKIP_TASKBAR` keeps it out of the task list,
//! `_NET_WM_WINDOW_TYPE_UTILITY` marks it as a tool window. All of that is a
//! *request*, honoured by the window manager or not.
//!
//! An **override-redirect** window is one the X server hands straight to the
//! client: the window manager is never told it exists. Scrozz's overlay
//! viewport asks winit for exactly this, because it is the only way to stop a
//! tiling window manager tiling the capture stack. The consequence is the part
//! that is easy to get wrong:
//!
//! > **Every EWMH property is dead on an override-redirect window.** Setting
//! > `_NET_WM_STATE` does nothing. Sending a `_NET_WM_STATE` client message to
//! > the root window does nothing. There is nobody listening.
//!
//! Code that sets those properties anyway and reports success is precisely the
//! silent no-op this crate exists to avoid. So [`plan_for`] returns an empty
//! state list for override-redirect windows and says so, and the things that
//! *do* work — restacking with `ConfigureWindow`, taking focus with
//! `SetInputFocus`, shaping input with `SHAPE` — are flagged as the client's own
//! responsibility.
//!
//! Everything here is arithmetic and byte layout with no X connection involved,
//! which is what makes it testable on a machine with no X server. [`super::x11`]
//! is the thin layer that turns it into requests.

use crate::overlay::{OverlayBehavior, OverlayLevel};

/// Whether the window manager can see this window at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Managed {
    /// An ordinary window. EWMH applies.
    ByWindowManager,
    /// `override_redirect = True`. The window manager is not involved, so EWMH
    /// does not apply and the client does its own stacking and focus.
    OverrideRedirect,
}

/// The `_NET_WM_WINDOW_TYPE` Scrozz asks for.
///
/// The type must be set *before* the window is mapped for a window manager to
/// act on it — most ignore a late change — so the real request is made by
/// `egui::ViewportBuilder::with_window_type`, and this enum exists so the
/// retrofit can say what it expects and check rather than guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowType {
    /// `_NET_WM_WINDOW_TYPE_UTILITY` — a tool window. Kept off the taskbar by
    /// most window managers and stacked above its own application.
    Utility,
    /// `_NET_WM_WINDOW_TYPE_DOCK` — a panel. Stacked above ordinary windows and
    /// never focused, which is why it suits an overlay that must not steal
    /// keystrokes.
    Dock,
    /// `_NET_WM_WINDOW_TYPE_NORMAL`.
    Normal,
}

impl WindowType {
    /// The atom name to intern.
    #[must_use]
    pub const fn atom_name(self) -> &'static str {
        match self {
            Self::Utility => "_NET_WM_WINDOW_TYPE_UTILITY",
            Self::Dock => "_NET_WM_WINDOW_TYPE_DOCK",
            Self::Normal => "_NET_WM_WINDOW_TYPE_NORMAL",
        }
    }
}

/// One `_NET_WM_STATE` atom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WmState {
    /// Stacked above ordinary windows.
    Above,
    /// Absent from the task list.
    SkipTaskbar,
    /// Absent from the pager / workspace switcher.
    SkipPager,
    /// Visible on every workspace.
    Sticky,
    /// Covers the whole output, over panels.
    Fullscreen,
}

impl WmState {
    /// The atom name to intern.
    #[must_use]
    pub const fn atom_name(self) -> &'static str {
        match self {
            Self::Above => "_NET_WM_STATE_ABOVE",
            Self::SkipTaskbar => "_NET_WM_STATE_SKIP_TASKBAR",
            Self::SkipPager => "_NET_WM_STATE_SKIP_PAGER",
            Self::Sticky => "_NET_WM_STATE_STICKY",
            Self::Fullscreen => "_NET_WM_STATE_FULLSCREEN",
        }
    }
}

/// `_NET_WM_STATE` client-message actions, as defined by EWMH.
pub mod state_action {
    /// Remove the state.
    pub const REMOVE: u32 = 0;
    /// Add the state.
    pub const ADD: u32 = 1;
    /// Toggle the state.
    pub const TOGGLE: u32 = 2;
}

/// `_NET_WM_DESKTOP` value meaning "all desktops".
pub const ALL_DESKTOPS: u32 = 0xFFFF_FFFF;

/// Every atom the X11 backend interns, in one place.
///
/// Interning is a round trip each, so the backend sends them as one batch; this
/// list is what it batches. Keeping it here rather than inline means a state
/// added to [`plan_for`] and forgotten here shows up as a failing test instead
/// of a `BadAtom` at runtime.
#[must_use]
pub fn required_atoms() -> Vec<&'static str> {
    let mut atoms = vec![
        "_NET_WM_STATE",
        "_NET_WM_WINDOW_TYPE",
        "_NET_WM_DESKTOP",
        "_NET_WORKAREA",
        "_NET_CURRENT_DESKTOP",
        "_NET_WM_USER_TIME",
        "WM_HINTS",
    ];
    for state in [
        WmState::Above,
        WmState::SkipTaskbar,
        WmState::SkipPager,
        WmState::Sticky,
        WmState::Fullscreen,
    ] {
        atoms.push(state.atom_name());
    }
    for kind in [WindowType::Utility, WindowType::Dock, WindowType::Normal] {
        atoms.push(kind.atom_name());
    }
    atoms
}

/// Everything the X11 backend will do to one window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X11Plan {
    /// The window type Scrozz expects the window to have been created with.
    pub window_type: WindowType,
    /// `_NET_WM_STATE` atoms to add. Empty for an override-redirect window,
    /// because nothing would read them.
    pub states: Vec<WmState>,
    /// Whether to set `_NET_WM_DESKTOP` to [`ALL_DESKTOPS`].
    pub all_desktops: bool,
    /// Whether the client must restack the window itself with
    /// `ConfigureWindow(stack_mode: Above)`.
    ///
    /// Always true for override-redirect windows, where it is the only stacking
    /// mechanism available. Also true for managed windows, where it is a cheap
    /// belt-and-braces alongside `_NET_WM_STATE_ABOVE`.
    pub client_restacks: bool,
    /// Whether the window should be able to take keyboard focus.
    ///
    /// For a managed window this is expressed by clearing the `input` flag in
    /// `WM_HINTS`, which is how a client says "do not focus me" in the ICCCM
    /// focus model. For an override-redirect window it means the client calls
    /// `SetInputFocus` itself, because no window manager will.
    pub takes_focus: bool,
    /// Whether the client must call `SetInputFocus` to get the keyboard.
    pub client_focuses: bool,
    /// Caveats worth showing a human, in the order they were decided.
    pub notes: Vec<&'static str>,
}

/// Decides what to do to a window, given how it was created.
///
/// The `managed` argument is not a preference — it is read from the window's own
/// attributes with `GetWindowAttributes`, because winit's override-redirect hint
/// is exactly that, a hint, and X11 is the one platform where the answer can be
/// checked rather than assumed.
#[must_use]
pub fn plan_for(behavior: &OverlayBehavior, managed: Managed) -> X11Plan {
    let fullscreen = matches!(
        behavior.level,
        OverlayLevel::Shielding | OverlayLevel::AboveMenuBar
    );
    let window_type = if fullscreen {
        // A fullscreen shield is not a tool palette; DOCK keeps it above panels
        // on window managers that stack by type.
        WindowType::Dock
    } else if behavior.level == OverlayLevel::Normal {
        WindowType::Normal
    } else {
        WindowType::Utility
    };

    match managed {
        Managed::OverrideRedirect => X11Plan {
            window_type,
            states: Vec::new(),
            all_desktops: false,
            client_restacks: true,
            takes_focus: behavior.accepts_key,
            client_focuses: behavior.accepts_key,
            notes: vec![
                "override-redirect: the window manager does not manage this window, so \
                 _NET_WM_STATE, _NET_WM_WINDOW_TYPE and _NET_WM_DESKTOP have no effect",
                "stacking is done by the client with ConfigureWindow, and focus with \
                 SetInputFocus",
            ],
        },
        Managed::ByWindowManager => {
            let mut states = Vec::new();
            if behavior.level > OverlayLevel::Normal {
                states.push(WmState::Above);
            }
            if !behavior.movable || behavior.ignore_cycle {
                states.push(WmState::SkipTaskbar);
                states.push(WmState::SkipPager);
            }
            if behavior.join_all_spaces {
                states.push(WmState::Sticky);
            }
            if fullscreen {
                states.push(WmState::Fullscreen);
            }

            X11Plan {
                window_type,
                states,
                all_desktops: behavior.join_all_spaces,
                client_restacks: true,
                takes_focus: behavior.accepts_key,
                client_focuses: false,
                notes: vec![
                    "managed window: every property below is a request the window manager \
                     may decline",
                    "_NET_WM_WINDOW_TYPE is honoured only if it was set before the window \
                     was mapped, which is why the viewport sets it at creation",
                ],
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WM_HINTS
// ---------------------------------------------------------------------------

/// Bit 0 of `WM_HINTS.flags`: the `input` field is meaningful.
pub const WM_HINTS_INPUT_FLAG: u32 = 1;

/// Number of 32-bit words in an ICCCM `WM_HINTS` property.
pub const WM_HINTS_WORDS: usize = 9;

/// Rewrites a `WM_HINTS` property to set or clear the `input` field.
///
/// `WM_HINTS` is nine 32-bit words — flags, input, initial_state, icon_pixmap,
/// icon_window, icon_x, icon_y, icon_mask, window_group — and a client that
/// wants to be unfocusable clears `input` while leaving the rest alone. winit
/// has already written a `WM_HINTS` for the window, so the existing bytes are
/// read, one field is changed, and everything else is preserved: replacing the
/// whole property with a fresh one would silently drop the window group, which
/// is what tells the window manager the overlay belongs to Scrozz.
///
/// `existing` may be `None` (no property yet) or the wrong length (a property
/// from a different client, or a truncated read); either way the result is a
/// well-formed nine-word property, because a malformed `WM_HINTS` is worse than
/// a default one.
///
/// Returns native-endian bytes, matching the byte order `x11rb` negotiates.
#[must_use]
pub fn encode_wm_hints_input(existing: Option<&[u8]>, input: bool) -> Vec<u8> {
    let mut words = [0u32; WM_HINTS_WORDS];
    if let Some(bytes) = existing {
        for (slot, chunk) in words.iter_mut().zip(bytes.as_chunks::<4>().0) {
            *slot = u32::from_ne_bytes(*chunk);
        }
    }

    words[0] |= WM_HINTS_INPUT_FLAG;
    words[1] = u32::from(input);

    words.iter().flat_map(|w| w.to_ne_bytes()).collect()
}

/// Reads the `input` field out of a `WM_HINTS` property.
///
/// Returns `None` when the property is absent, too short, or does not claim the
/// `input` field is meaningful — three different ways of saying "the client did
/// not express a preference", all of which mean the same thing to a window
/// manager.
#[must_use]
pub fn decode_wm_hints_input(bytes: &[u8]) -> Option<bool> {
    let mut words = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_ne_bytes(*chunk));
    let flags = words.next()?;
    let input = words.next()?;
    if flags & WM_HINTS_INPUT_FLAG == 0 {
        return None;
    }
    Some(input != 0)
}

// ---------------------------------------------------------------------------
// _NET_WORKAREA
// ---------------------------------------------------------------------------

/// A rectangle as EWMH reports it: signed origin, unsigned extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireRect {
    /// Left edge in root coordinates.
    pub x: i32,
    /// Top edge in root coordinates.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Extracts one desktop's work area from `_NET_WORKAREA`.
///
/// The property is `CARDINAL[4 * n]` — x, y, width, height once per virtual
/// desktop — so the current desktop index selects the right quadruple. Values
/// are read as signed because window managers do emit negative origins for
/// off-origin monitor layouts.
///
/// This is the property that keeps the capture stack off the panel. Anchoring to
/// raw screen bounds instead puts it underneath a KDE panel or a GNOME dock,
/// where it is invisible and unclickable — and the bug reads as the overlay
/// never appearing, not as a placement error.
///
/// Returns `None` when the property is absent, empty, or too short for the
/// requested desktop, so the caller can fall back to full screen bounds: wrong
/// but visible, rather than wrong and hidden.
#[must_use]
pub fn parse_work_area(bytes: &[u8], desktop: u32) -> Option<WireRect> {
    let values: Vec<i32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| i32::from_ne_bytes(*chunk))
        .collect();

    let base = (desktop as usize).checked_mul(4)?;
    let quad = values.get(base..base.checked_add(4)?)?;
    let width = u32::try_from(quad[2]).ok()?;
    let height = u32::try_from(quad[3]).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some(WireRect {
        x: quad[0],
        y: quad[1],
        width,
        height,
    })
}
