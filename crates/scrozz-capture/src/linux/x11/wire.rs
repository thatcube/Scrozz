//! Hand-encoded RandR requests and replies.
//!
//! # Why this is hand-rolled
//!
//! RandR is not optional: without it a multi-monitor desktop looks like one
//! enormous screen, every per-monitor bound is wrong, and there is no primary
//! display. Scrozz's implementation predates enabling `x11rb`'s generated RandR
//! module and deliberately keeps only the two requests it needs in this small,
//! host-testable parser.
//!
//! So the two requests that are actually needed are encoded here directly. This
//! is not a workaround so much as a relocation of the same bytes: the layouts
//! below are transcribed from `x11rb-protocol`'s own generated `randr.rs`, which
//! is generated in turn from the X.Org XML protocol description.
//!
//! # Why that is a good trade rather than a regrettable one
//!
//! Generated bindings cannot be exercised on a machine with no X server. These
//! functions can: they map `&[u8]` to values and back, with no connection
//! involved, so `tests/linux.rs` runs them on macOS against byte buffers built
//! to the specification. The riskiest part of the extension — the wire layout —
//! ends up **more** covered than it would have been, not less.
//!
//! # Byte order
//!
//! `x11rb` performs the connection handshake in the host's native byte order and
//! the server replies in kind, so every multi-byte field here is native-endian.

/// The extension name to pass to `RequestConnection::extension_information`.
pub const RANDR_EXTENSION_NAME: &str = "RANDR";

/// Minor opcode of `RRQueryVersion`.
pub const QUERY_VERSION_OPCODE: u8 = 0;

/// Minor opcode of `RRGetMonitors`.
pub const GET_MONITORS_OPCODE: u8 = 42;

/// The RandR version `RRGetMonitors` was introduced in.
pub const MONITORS_SINCE: (u32, u32) = (1, 5);

/// Encodes `RRQueryVersion`.
///
/// The client states the highest version it understands; the server replies with
/// the highest it and the client have in common. Asking for 1.5 and being told
/// 1.4 is the normal way to discover that `RRGetMonitors` is unavailable —
/// calling it anyway earns a `BadRequest` that is indistinguishable from a bug.
#[must_use]
pub fn query_version_request(major_opcode: u8, major: u32, minor: u32) -> [u8; 12] {
    let mut request = [0u8; 12];
    request[0] = major_opcode;
    request[1] = QUERY_VERSION_OPCODE;
    request[2..4].copy_from_slice(&3u16.to_ne_bytes());
    request[4..8].copy_from_slice(&major.to_ne_bytes());
    request[8..12].copy_from_slice(&minor.to_ne_bytes());
    request
}

/// Encodes `RRGetMonitors`.
///
/// `active_only` asks the server to omit monitors with no enabled outputs, which
/// is what a screenshot tool wants: a disabled monitor has no pixels.
#[must_use]
pub fn get_monitors_request(major_opcode: u8, window: u32, active_only: bool) -> [u8; 12] {
    let mut request = [0u8; 12];
    request[0] = major_opcode;
    request[1] = GET_MONITORS_OPCODE;
    request[2..4].copy_from_slice(&3u16.to_ne_bytes());
    request[4..8].copy_from_slice(&window.to_ne_bytes());
    request[8] = u8::from(active_only);
    request
}

/// A malformed or truncated reply.
///
/// Deliberately opaque: nothing can be done about a server that answers
/// nonsense except decline to trust the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireError;

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("malformed X11 reply")
    }
}

impl std::error::Error for WireError {}

/// The negotiated RandR version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    /// Major version.
    pub major: u32,
    /// Minor version.
    pub minor: u32,
}

impl Version {
    /// Whether `RRGetMonitors` may be called.
    #[must_use]
    pub const fn supports_monitors(&self) -> bool {
        self.major > MONITORS_SINCE.0
            || (self.major == MONITORS_SINCE.0 && self.minor >= MONITORS_SINCE.1)
    }
}

/// One monitor as RandR 1.5 describes it.
///
/// A *monitor* is not an *output*: RandR 1.5 introduced monitors precisely so
/// that a desktop spanning two outputs in one framebuffer, or one output split
/// into two logical halves, has a first-class name. It is the right unit for a
/// screenshot tool, and it is a single round trip rather than the
/// screen-resources-then-crtc-per-output dance RandR 1.2 requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    /// Atom holding the monitor's name; resolve with `GetAtomName`.
    pub name: u32,
    /// Whether this is the primary monitor.
    pub primary: bool,
    /// Whether the monitor was created automatically from outputs.
    pub automatic: bool,
    /// Left edge in root coordinates.
    pub x: i32,
    /// Top edge in root coordinates.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Physical width in millimetres, frequently fabricated by the display.
    pub width_mm: u32,
    /// Physical height in millimetres, equally frequently fabricated.
    pub height_mm: u32,
    /// RandR output ids backing this monitor.
    pub outputs: Vec<u32>,
}

/// Parses an `RRQueryVersion` reply body.
///
/// Layout, from offset 0 of the reply: `u8` response type, 1 pad, `u16`
/// sequence, `u32` length, `u32` major, `u32` minor, 16 pad.
///
/// # Errors
///
/// Returns [`WireError`] if the buffer is short or is not a reply.
pub fn parse_query_version(reply: &[u8]) -> Result<Version, WireError> {
    if reply.len() < 32 || reply[0] != 1 {
        return Err(WireError);
    }
    Ok(Version {
        major: read_u32(reply, 8)?,
        minor: read_u32(reply, 12)?,
    })
}

/// Parses an `RRGetMonitors` reply body.
///
/// Layout, from offset 0: `u8` response type, 1 pad, `u16` sequence, `u32`
/// length, `u32` timestamp, `u32` monitor count, `u32` output count, 12 pad,
/// then the monitors. Each monitor is 24 fixed bytes followed by its output
/// list, so the records are variable-length and must be walked rather than
/// indexed — the classic place to introduce an off-by-one, and the reason this
/// function exists as something testable.
///
/// # Errors
///
/// Returns [`WireError`] if the buffer is short or is not a reply.
pub fn parse_monitors(reply: &[u8]) -> Result<Vec<Monitor>, WireError> {
    if reply.len() < 32 || reply[0] != 1 {
        return Err(WireError);
    }
    let count = read_u32(reply, 12)? as usize;

    let mut monitors = Vec::with_capacity(count.min(64));
    let mut offset = 32usize;

    for _ in 0..count {
        let end = offset.checked_add(24).ok_or(WireError)?;
        if end > reply.len() {
            return Err(WireError);
        }
        let name = read_u32(reply, offset)?;
        let primary = reply[offset + 4] != 0;
        let automatic = reply[offset + 5] != 0;
        let output_count = read_u16(reply, offset + 6)? as usize;
        let x = read_i16(reply, offset + 8)?;
        let y = read_i16(reply, offset + 10)?;
        let width = read_u16(reply, offset + 12)?;
        let height = read_u16(reply, offset + 14)?;
        let width_mm = read_u32(reply, offset + 16)?;
        let height_mm = read_u32(reply, offset + 20)?;

        let outputs_bytes = output_count.checked_mul(4).ok_or(WireError)?;
        let outputs_end = end.checked_add(outputs_bytes).ok_or(WireError)?;
        if outputs_end > reply.len() {
            return Err(WireError);
        }
        let outputs = (0..output_count)
            .map(|i| read_u32(reply, end + i * 4))
            .collect::<Result<Vec<_>, _>>()?;

        monitors.push(Monitor {
            name,
            primary,
            automatic,
            x: i32::from(x),
            y: i32::from(y),
            width: u32::from(width),
            height: u32::from(height),
            width_mm,
            height_mm,
            outputs,
        });

        offset = outputs_end;
    }

    Ok(monitors)
}

/// Chooses which monitor is primary when the server marks none.
///
/// RandR permits zero primaries — it happens on a fresh `xrandr --output ...`
/// that never set one — and `Display::is_primary` being false for every display
/// leaves callers with no anchor for a default. Falling back to the monitor at
/// the root origin matches what desktop environments do.
#[must_use]
pub fn primary_index(monitors: &[Monitor]) -> Option<usize> {
    if monitors.is_empty() {
        return None;
    }
    monitors
        .iter()
        .position(|m| m.primary)
        .or_else(|| monitors.iter().position(|m| m.x == 0 && m.y == 0))
        .or(Some(0))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, WireError> {
    let slice = bytes.get(at..at + 4).ok_or(WireError)?;
    Ok(u32::from_ne_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, WireError> {
    let slice = bytes.get(at..at + 2).ok_or(WireError)?;
    Ok(u16::from_ne_bytes([slice[0], slice[1]]))
}

fn read_i16(bytes: &[u8], at: usize) -> Result<i16, WireError> {
    let slice = bytes.get(at..at + 2).ok_or(WireError)?;
    Ok(i16::from_ne_bytes([slice[0], slice[1]]))
}
