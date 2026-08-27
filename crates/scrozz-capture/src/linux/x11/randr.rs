//! Sending the hand-encoded RandR requests over an `x11rb` connection.
//!
//! Everything that can be tested without a server lives in [`super::wire`]; this
//! file is the thin, untestable seam between those bytes and the socket.

use x11rb::connection::RequestConnection;
use x11rb::errors::ParseError;
use x11rb::x11_utils::TryParse;

use super::wire::{self, Monitor, Version};

/// A reply captured as raw bytes.
///
/// `x11rb` requires the reply type to implement [`TryParse`], but the whole
/// point here is to parse in pure code that a Mac can run. So this parses only
/// as much as the transport demands — the length header, which tells `x11rb`
/// where the reply ends — and hands the bytes on untouched.
struct RawReply(Vec<u8>);

impl TryParse for RawReply {
    fn try_parse(value: &[u8]) -> Result<(Self, &[u8]), ParseError> {
        let header = value.get(..8).ok_or(ParseError::InsufficientData)?;
        let extra = u32::from_ne_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let total = extra
            .checked_mul(4)
            .and_then(|n| n.checked_add(32))
            .ok_or(ParseError::InvalidValue)?;
        let bytes = value.get(..total).ok_or(ParseError::InsufficientData)?;
        Ok((Self(bytes.to_vec()), &value[total..]))
    }
}

/// A connection that has been confirmed to speak RandR 1.5 or later.
///
/// Constructing one is the only way to call [`Self::monitors`], so a version
/// check cannot be forgotten — the failure mode it prevents is a `BadRequest`
/// on servers old enough to lack `RRGetMonitors`, which surfaces as an opaque
/// protocol error rather than as "your X server is too old".
#[derive(Debug, Clone, Copy)]
pub struct RandrExtension {
    major_opcode: u8,
    version: Version,
}

impl RandrExtension {
    /// Queries for RandR and negotiates a version.
    ///
    /// Returns `Ok(None)` when the extension is absent or too old, which is a
    /// legitimate configuration (a bare `Xvfb`, an ancient server, some remote
    /// X implementations) and not an error — the caller falls back to treating
    /// the root window as a single display.
    ///
    /// # Errors
    ///
    /// Returns an error only if the connection itself fails.
    pub fn query<C: RequestConnection>(conn: &C) -> Result<Option<Self>, ConnError> {
        let Some(info) = conn.extension_information(wire::RANDR_EXTENSION_NAME)? else {
            return Ok(None);
        };

        let request = wire::query_version_request(
            info.major_opcode,
            wire::MONITORS_SINCE.0,
            wire::MONITORS_SINCE.1,
        );
        let reply = send(conn, &request)?;
        let version = wire::parse_query_version(&reply.0).map_err(|_| ConnError::Malformed)?;

        Ok(version.supports_monitors().then_some(Self {
            major_opcode: info.major_opcode,
            version,
        }))
    }

    /// The version the server agreed to.
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// Fetches the active monitors on a screen.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection fails or the reply is malformed.
    pub fn monitors<C: RequestConnection>(
        &self,
        conn: &C,
        root: u32,
    ) -> Result<Vec<Monitor>, ConnError> {
        let request = wire::get_monitors_request(self.major_opcode, root, true);
        let reply = send(conn, &request)?;
        wire::parse_monitors(&reply.0).map_err(|_| ConnError::Malformed)
    }
}

fn send<C: RequestConnection>(conn: &C, request: &[u8]) -> Result<RawReply, ConnError> {
    let buf = [std::io::IoSlice::new(request)];
    conn.send_request_with_reply::<RawReply>(&buf, Vec::new())?
        .reply()
        .map_err(|_| ConnError::Malformed)
}

/// Why a RandR call could not be completed.
#[derive(Debug)]
pub enum ConnError {
    /// The X connection failed.
    Connection(x11rb::errors::ConnectionError),
    /// The server's reply did not match the protocol.
    Malformed,
}

impl From<x11rb::errors::ConnectionError> for ConnError {
    fn from(err: x11rb::errors::ConnectionError) -> Self {
        Self::Connection(err)
    }
}

impl std::fmt::Display for ConnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(err) => write!(f, "X11 connection failed: {err}"),
            Self::Malformed => f.write_str("the X server sent a malformed RandR reply"),
        }
    }
}

impl std::error::Error for ConnError {}
