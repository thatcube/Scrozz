//! `xdg-desktop-portal` negotiation.
//!
//! Everything here that can be decided without D-Bus is decided here as plain
//! values and tested. The still-capture backend has not yet adopted the
//! PipeWire consumer used by `scrozz-record`; [`acquire_frame`] marks that
//! boundary without making stale claims about Cargo features.
//!
//! # The two portal interfaces, and why both matter
//!
//! `org.freedesktop.portal.Screenshot` takes a picture and hands back a `file://`
//! URI. It is one call, needs no PipeWire, and is what a "screenshot the whole
//! screen" command should use. What it cannot do is let the application choose
//! the region or the window: `interactive: true` delegates the entire selection
//! to the desktop's own UI, which means Scrozz's editor, magnifier and
//! measurement overlay never appear. It is a correct fallback, not the product.
//!
//! `org.freedesktop.portal.ScreenCast` gives a PipeWire node the application
//! reads frames from, with the selection made once and then restored silently on
//! later captures. It is the only route to Scrozz's own selection UI, and it is
//! the route this module is built around.
//!
//! # Wire constants
//!
//! Transcribed from the portal specification and cross-checked against the
//! enabled `ashpd` definitions.

use scrozz_core::CaptureTarget;

use super::restore::TokenKey;

/// `SourceType` bit flags from the ScreenCast specification.
pub mod source_type {
    /// A whole monitor.
    pub const MONITOR: u32 = 1;
    /// A single window, chosen in the portal's own picker.
    pub const WINDOW: u32 = 2;
    /// A virtual, compositor-synthesised source.
    pub const VIRTUAL: u32 = 4;
}

/// `CursorMode` bit flags from the ScreenCast specification.
pub mod cursor_mode {
    /// The pointer is absent from the stream.
    pub const HIDDEN: u32 = 1;
    /// The pointer is drawn into the stream's buffers.
    pub const EMBEDDED: u32 = 2;
    /// The pointer is delivered as stream metadata for the client to draw.
    pub const METADATA: u32 = 4;
}

/// `PersistMode` values from the ScreenCast specification.
pub mod persist_mode {
    /// No token is issued; the user is asked every time.
    pub const DO_NOT: u32 = 0;
    /// The grant lasts as long as the process does.
    pub const APPLICATION: u32 = 1;
    /// The grant lasts until the user revokes it in system settings.
    pub const EXPLICITLY_REVOKED: u32 = 2;
}

/// The options for one `SelectSources` call.
///
/// Produced entirely from the request, with no D-Bus involved, so the decisions
/// — which source types to offer, whether to embed the pointer, which stored
/// token to present — are testable on any machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPlan {
    /// Bit mask of `SourceType` values to offer in the picker.
    pub types: u32,
    /// The `CursorMode` to request.
    pub cursor: u32,
    /// Whether the user may pick more than one source.
    pub multiple: bool,
    /// The `PersistMode` to request.
    pub persist: u32,
    /// Which stored token, if any, to present for silent restore.
    pub restore_key: TokenKey,
}

impl SessionPlan {
    /// Builds the plan for a capture request.
    ///
    /// Two choices here are deliberate:
    ///
    /// - The pointer is requested **embedded** rather than as metadata when it
    ///   is wanted. Metadata is better for a recorder, which can draw a crisp
    ///   pointer at any scale, but a still capture would have to composite it
    ///   itself and would get the hotspot subtly wrong.
    /// - Persistence is always `EXPLICITLY_REVOKED`. `APPLICATION` sounds safer
    ///   but expires when the process does, so every launch costs a permission
    ///   dialog — which is the failure this whole mechanism exists to prevent.
    #[must_use]
    pub fn for_target(target: &CaptureTarget, want_cursor: bool) -> Self {
        let (types, restore_key) = match target {
            CaptureTarget::Window(_) => (source_type::WINDOW, TokenKey::Window),
            CaptureTarget::AllDisplays => (
                source_type::MONITOR | source_type::VIRTUAL,
                TokenKey::AllDisplays,
            ),
            // A region is cropped from a monitor capture after the fact.
            // The portal has no concept of a sub-rectangle, and asking the user
            // to pick a region in the portal's UI and then again in Scrozz's
            // would be absurd.
            CaptureTarget::Display(_) | CaptureTarget::Region(_) => {
                (source_type::MONITOR, TokenKey::Monitor)
            }
        };

        Self {
            types,
            cursor: if want_cursor {
                cursor_mode::EMBEDDED
            } else {
                cursor_mode::HIDDEN
            },
            multiple: matches!(target, CaptureTarget::AllDisplays),
            persist: persist_mode::EXPLICITLY_REVOKED,
            restore_key,
        }
    }

    /// Narrows the plan to what the portal says it can actually do.
    ///
    /// Decision D8 requires capability by query rather than assumption, and the
    /// ScreenCast interface publishes `AvailableSourceTypes` and
    /// `AvailableCursorModes` for exactly this. Requesting an unavailable bit is
    /// not ignored — the portal rejects the call — so a compositor that cannot
    /// offer window sources must be met with a narrowed request and, if nothing
    /// remains, a truthful refusal.
    ///
    /// Returns `None` when the intersection is empty, which the caller turns
    /// into [`scrozz_core::Error::Unsupported`].
    #[must_use]
    pub fn narrow(mut self, available_types: u32, available_cursors: u32) -> Option<Self> {
        self.types &= available_types;
        if self.types == 0 {
            return None;
        }
        if self.cursor & available_cursors == 0 {
            // Every portal implements Hidden, so falling back to it keeps the
            // capture working and loses only the pointer.
            self.cursor = cursor_mode::HIDDEN;
        }
        Some(self)
    }
}

/// Turns a portal `file://` URI into a path.
///
/// The Screenshot interface answers with a URI, not a path, and the difference
/// bites: a screenshot saved while the user's locale produced a filename with a
/// space arrives as `%20`, and opening the literal `%20` path fails with "no
/// such file" on a file that plainly exists.
///
/// Rejects anything that is not `file://`; the portal may legitimately hand back
/// a `file:` URI pointing into its own document store, but a `http://` here
/// would mean something has gone very wrong.
#[must_use]
pub fn path_from_uri(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file://host/path` is legal; an empty or `localhost` authority is the
    // only form a portal produces, and both leave the path starting at the
    // first slash.
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(index) if &rest[..index] == "localhost" => &rest[index..],
        _ => return None,
    };

    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }

    Some(std::path::PathBuf::from(String::from_utf8(out).ok()?))
}

/// One video stream as `Start` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    /// The PipeWire node to connect to.
    pub node_id: u32,
    /// Position in the compositor's global space, if the portal supplied one.
    ///
    /// Optional in the specification and genuinely absent on some compositors
    /// for window streams, where the window's desktop position is not something
    /// the client is allowed to know.
    pub position: Option<(i32, i32)>,
    /// Size in pixels, if supplied.
    pub size: Option<(i32, i32)>,
}

impl StreamInfo {
    /// Whether the stream carries enough geometry to place it on the desktop.
    #[must_use]
    pub const fn is_placeable(&self) -> bool {
        self.position.is_some() && self.size.is_some()
    }
}

/// Reads pixels from a PipeWire node.
///
/// # Not implemented in the still-capture crate
///
/// The ashpd ScreenCast and Screenshot features are enabled. Turning `Start`'s
/// node id into pixels still requires PipeWire system libraries; those are
/// intentionally housed in `scrozz-record`'s non-default `linux-native`
/// feature. Static capture will share that consumer when this crate grows an
/// equivalent optional-native boundary.
///
/// # Panics
///
/// Always.
#[allow(clippy::needless_pass_by_value)]
pub fn acquire_frame(_stream: StreamInfo) -> ! {
    todo!(
        "still-image PipeWire acquisition is not wired in scrozz-capture; \
         scrozz-record owns the working continuous PipeWire consumer"
    )
}
