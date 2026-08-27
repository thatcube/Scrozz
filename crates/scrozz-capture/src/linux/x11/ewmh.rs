//! Parsers for the EWMH and ICCCM window properties the X11 backend reads.
//!
//! X11 hands properties back as an untyped byte buffer plus a format width, so
//! every one of these is a slice-of-bytes-to-meaning function with no X
//! connection involved. That is what makes them testable on a machine with no X
//! server, which is the whole point: property parsing is where the off-by-one
//! and the endianness mistake live, and it is otherwise the least verifiable
//! code in the backend.
//!
//! **Byte order.** `x11rb` performs the connection handshake in the machine's
//! native byte order, so the server answers in native order too. Every 32-bit
//! read below is therefore `from_ne_bytes`, and that is correct rather than
//! merely convenient.
//!
//! `tests/linux.rs` includes this file directly by path so these run everywhere.

/// Reads a `CARDINAL[]`/`WINDOW[]` property body as 32-bit items.
///
/// Trailing bytes that do not complete an item are ignored rather than treated
/// as an error: a short final chunk means the property changed underneath the
/// read, which is normal on a live desktop and not worth failing a whole
/// enumeration over.
#[must_use]
pub fn parse_u32_list(bytes: &[u8]) -> Vec<u32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_ne_bytes(*c))
        .collect()
}

/// Reads a `CARDINAL[]` property body as signed 32-bit items.
///
/// `_NET_FRAME_EXTENTS` and `_NET_WORKAREA` are nominally `CARDINAL`, but window
/// managers do emit negative values for off-origin monitor layouts, so the
/// signed read is the honest one.
#[must_use]
pub fn parse_i32_list(bytes: &[u8]) -> Vec<i32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| i32::from_ne_bytes(*c))
        .collect()
}

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

impl WireRect {
    /// Whether the rectangle encloses any area.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// Extracts one desktop's work area from `_NET_WORKAREA`.
///
/// The property is `CARDINAL[4 * n]` — x, y, width, height repeated once per
/// virtual desktop — so the current desktop index selects the right quadruple.
///
/// This is the property that keeps a floating overlay off the panel. Anchoring
/// to the raw screen bounds instead puts the capture stack underneath a GNOME
/// dock or a KDE panel, where it is invisible and unclickable, and the bug looks
/// like the overlay failing to appear at all.
///
/// Returns `None` when the property is absent, empty, or too short for the
/// requested desktop; the caller then falls back to full screen bounds, which is
/// wrong but visible rather than wrong and hidden.
#[must_use]
pub fn parse_work_area(bytes: &[u8], desktop: u32) -> Option<WireRect> {
    let values = parse_i32_list(bytes);
    let base = (desktop as usize).checked_mul(4)?;
    let quad = values.get(base..base + 4)?;
    let rect = WireRect {
        x: quad[0],
        y: quad[1],
        width: u32::try_from(quad[2]).ok()?,
        height: u32::try_from(quad[3]).ok()?,
    };
    rect.is_valid().then_some(rect)
}

/// Decodes a `_NET_WM_NAME` value, which is `UTF8_STRING`.
///
/// Invalid sequences are replaced rather than rejected. A window with a
/// mis-encoded title is common — old Java and Wine applications produce them —
/// and dropping the window from the picker entirely is a much worse outcome for
/// the user than showing a title with a replacement character in it.
#[must_use]
pub fn parse_utf8_name(bytes: &[u8]) -> Option<String> {
    let trimmed = trim_trailing_nuls(bytes);
    if trimmed.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(trimmed).into_owned())
}

/// Decodes a legacy `WM_NAME` value, which is `STRING`, meaning Latin-1.
///
/// Used only when `_NET_WM_NAME` is absent. Decoding Latin-1 as UTF-8 mangles
/// every accented character, so the byte-to-`char` widening here is load-bearing
/// rather than pedantic.
#[must_use]
pub fn parse_latin1_name(bytes: &[u8]) -> Option<String> {
    let trimmed = trim_trailing_nuls(bytes);
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.iter().map(|&b| b as char).collect())
}

/// Splits `WM_CLASS` into its instance and class names.
///
/// The property is two NUL-terminated Latin-1 strings back to back. The second,
/// the class, is the one worth showing a user: it is `Firefox` where the
/// instance is `Navigator`.
///
/// Returns `(instance, class)`; either may be empty if the property is
/// malformed, which some toolkits do produce.
#[must_use]
pub fn parse_wm_class(bytes: &[u8]) -> Option<(String, String)> {
    if bytes.is_empty() {
        return None;
    }
    let mut parts = bytes.split(|&b| b == 0);
    let instance = parts.next().unwrap_or_default();
    let class = parts.next().unwrap_or_default();
    if instance.is_empty() && class.is_empty() {
        return None;
    }
    let latin1 = |s: &[u8]| s.iter().map(|&b| b as char).collect::<String>();
    Some((latin1(instance), latin1(class)))
}

/// The application name to show for a window, preferring the class.
///
/// Falls back to the instance, then to `None`, so a window with a broken
/// `WM_CLASS` is still listed rather than dropped.
#[must_use]
pub fn application_name(bytes: &[u8]) -> Option<String> {
    let (instance, class) = parse_wm_class(bytes)?;
    if !class.is_empty() {
        Some(class)
    } else if instance.is_empty() {
        None
    } else {
        Some(instance)
    }
}

/// Decoration insets from `_NET_FRAME_EXTENTS`, as `(left, right, top, bottom)`.
///
/// A client window's geometry excludes the title bar and borders the window
/// manager draws around it. Reporting that geometry as the window's bounds makes
/// every window appear to start below its own title bar, and a capture aimed at
/// those bounds crops the title bar off.
#[must_use]
pub fn parse_frame_extents(bytes: &[u8]) -> Option<(i32, i32, i32, i32)> {
    let values = parse_i32_list(bytes);
    let quad = values.get(0..4)?;
    Some((quad[0], quad[1], quad[2], quad[3]))
}

/// Grows a rectangle by frame extents, giving the frame rather than the client.
#[must_use]
pub fn apply_frame_extents(rect: WireRect, extents: (i32, i32, i32, i32)) -> WireRect {
    let (left, right, top, bottom) = extents;
    let x = rect.x.saturating_sub(left);
    let y = rect.y.saturating_sub(top);
    let width = i64::from(rect.width) + i64::from(left) + i64::from(right);
    let height = i64::from(rect.height) + i64::from(top) + i64::from(bottom);
    WireRect {
        x,
        y,
        width: u32::try_from(width.max(0)).unwrap_or(rect.width),
        height: u32::try_from(height.max(0)).unwrap_or(rect.height),
    }
}

/// The ICCCM `WM_STATE` values Scrozz cares about.
pub mod wm_state {
    /// The window is withdrawn — not managed, not on screen.
    pub const WITHDRAWN: u32 = 0;
    /// The window is mapped and visible.
    pub const NORMAL: u32 = 1;
    /// The window is minimised/iconified.
    pub const ICONIC: u32 = 3;
}

/// Reads the state word from `WM_STATE`.
///
/// The property is `(state, icon_window)`; only the first word matters here.
#[must_use]
pub fn parse_wm_state(bytes: &[u8]) -> Option<u32> {
    parse_u32_list(bytes).first().copied()
}

/// Whether a window should appear in the picker.
///
/// A minimised window has no current pixels to capture — `GetImage` on an
/// unmapped window fails with `BadMatch` — so listing it as capturable produces
/// a failure the user cannot act on.
#[must_use]
pub fn is_listable(state: Option<u32>, mapped: bool) -> bool {
    match state {
        Some(wm_state::NORMAL) => mapped,
        Some(wm_state::ICONIC | wm_state::WITHDRAWN) => false,
        // No `WM_STATE` means the window is not managed by the window manager:
        // an override-redirect window such as a menu, tooltip or notification.
        // Those are legitimately capturable when mapped.
        _ => mapped,
    }
}

/// Orders a stacking list front-most first.
///
/// `_NET_CLIENT_LIST_STACKING` is defined bottom-to-top, and
/// [`scrozz_core::TargetEnumerator::windows`] is specified front-most first, so
/// the reversal is part of the contract rather than a preference. Getting it
/// backwards means the picker offers the desktop wallpaper as the first choice.
#[must_use]
pub fn stacking_to_front_first(mut stacking: Vec<u32>) -> Vec<u32> {
    stacking.reverse();
    stacking
}

fn trim_trailing_nuls(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|&b| b != 0)
        .map_or(0, |index| index + 1);
    &bytes[..end]
}
