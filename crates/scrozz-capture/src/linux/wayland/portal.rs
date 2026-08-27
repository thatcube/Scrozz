//! `xdg-desktop-portal` negotiation.
//!
//! Everything here that can be decided without D-Bus is decided here, as plain
//! values, and tested on every platform. The D-Bus calls themselves live in
//! [`super::screencast`], and the pixels they lead to in
//! [`super::pipewire`].
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
//! Transcribed from the portal specification. `ashpd` has its own copies and
//! this module is deliberately not built on them: these values are what the
//! negotiation reasons about, and keeping them as plain integers is what lets
//! [`SessionPlan`] be tested on a machine with no portal, no D-Bus and no
//! Linux.

use scrozz_core::{CaptureTarget, Error};

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
    /// The `PersistMode` to request.
    pub persist: u32,
    /// Which stored token, if any, to present for silent restore.
    pub restore_key: TokenKey,
}

/// A capture target the portal cannot serve without guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanFailure {
    /// ScreenCast may return several streams but does not guarantee their
    /// positions, so they cannot always be composed into one desktop.
    AllDisplaysNeedsPositions,
}

impl PlanFailure {
    /// Turns the planning refusal into the public platform error.
    #[must_use]
    pub fn into_error(self) -> Error {
        match self {
            Self::AllDisplaysNeedsPositions => Error::Unsupported {
                what: "capturing all displays on Wayland".into(),
                why: "The ScreenCast portal may return one stream per monitor, but it does not \
                      guarantee each stream's desktop position. Scrozz cannot compose a correct \
                      virtual-desktop image without every position, so it refuses before opening \
                      the portal picker instead of capturing only the first display. Capture one \
                      display instead."
                    .into(),
            },
        }
    }
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
    ///
    /// # Errors
    ///
    /// Rejects all-display capture before any portal call. The portal makes
    /// per-stream positions optional, so requesting several sources could prompt
    /// the user and still leave no honest way to compose the result.
    pub fn for_target(
        target: &CaptureTarget,
        want_cursor: bool,
    ) -> std::result::Result<Self, PlanFailure> {
        let (types, restore_key) = match target {
            CaptureTarget::Window(_) => (source_type::WINDOW, TokenKey::Window),
            CaptureTarget::AllDisplays => return Err(PlanFailure::AllDisplaysNeedsPositions),
            // A region is cropped from a monitor capture after the fact.
            // The portal has no concept of a sub-rectangle, and asking the user
            // to pick a region in the portal's UI and then again in Scrozz's
            // would be absurd.
            CaptureTarget::Display(_) | CaptureTarget::Region(_) => {
                (source_type::MONITOR, TokenKey::Monitor)
            }
        };

        Ok(Self {
            types,
            cursor: if want_cursor {
                cursor_mode::EMBEDDED
            } else {
                cursor_mode::HIDDEN
            },
            persist: persist_mode::EXPLICITLY_REVOKED,
            restore_key,
        })
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

/// Why a portal negotiation did not produce a stream.
///
/// A plain enum rather than a `scrozz_core::Error` so that the classification —
/// which is the part with judgement in it — can be decided and tested without
/// D-Bus, and so that [`super::screencast`] has one obvious place to funnel
/// every `ashpd` error into.
///
/// The distinction that matters most is [`Self::Cancelled`]. A user who presses
/// Escape in the portal's picker has not encountered a fault, and a screenshot
/// tool that shows an error dialog when someone changes their mind is a
/// screenshot tool people stop using. D15 makes this an expected outcome; the
/// portal reports it as an ordinary D-Bus response with code 1, and it must
/// survive the whole way out as [`scrozz_core::Error::Cancelled`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortalFailure {
    /// The user dismissed the picker, or the compositor withdrew the request.
    Cancelled,
    /// No portal is running, or it does not implement ScreenCast.
    Missing(String),
    /// The portal is present but offers none of the sources this capture needs.
    NoSources {
        /// The source-type mask that was wanted.
        wanted: u32,
        /// The mask the portal advertised.
        available: u32,
    },
    /// The portal agreed but returned no video stream.
    NoStreams,
    /// `SelectSources` specifically rejected the supplied restore token.
    RestoreRejected(String),
    /// A D-Bus or protocol failure.
    Bus(String),
}

impl PortalFailure {
    /// Whether retrying once without the stored token can change this outcome.
    #[must_use]
    pub const fn should_retry_without_restore(&self) -> bool {
        matches!(self, Self::RestoreRejected(_))
    }

    /// Turns the failure into the error a caller should see.
    ///
    /// `compositor` is woven into the text because "screen capture failed" is
    /// useless and "wlroots' portal does not offer window sources" is
    /// actionable.
    #[must_use]
    pub fn into_error(self, compositor: &str) -> scrozz_core::Error {
        use scrozz_core::Error;

        match self {
            Self::Cancelled => Error::Cancelled,
            Self::Missing(detail) => Error::Unsupported {
                what: "capturing on Wayland".into(),
                why: format!(
                    "no desktop portal implementing ScreenCast answered on the session bus, and \
                     Wayland offers no other route to screen pixels. Install the portal for \
                     {compositor} — `xdg-desktop-portal-gnome` on GNOME, \
                     `xdg-desktop-portal-kde` on KDE, `xdg-desktop-portal-wlr` on wlroots \
                     compositors such as sway or Hyprland — alongside `xdg-desktop-portal` \
                     itself, then log out and back in. ({detail})"
                ),
            },
            Self::NoSources { wanted, available } => Error::Unsupported {
                what: describe_sources(wanted),
                why: format!(
                    "the portal on {compositor} advertises only {} and cannot offer {}. This is a \
                     limitation of the compositor's portal backend, not of the request",
                    describe_sources(available),
                    describe_sources(wanted & !available),
                ),
            },
            Self::NoStreams => Error::Platform(format!(
                "the portal on {compositor} approved the capture but returned no video stream, \
                 which means its screen-cast backend started and then produced nothing"
            )),
            Self::RestoreRejected(detail) => Error::Platform(format!(
                "the desktop portal on {compositor} rejected the stored screen-cast restore token: \
                 {detail}"
            )),
            Self::Bus(detail) => Error::Platform(format!(
                "the desktop portal on {compositor} failed: {detail}"
            )),
        }
    }
}

/// Recognises xdg-desktop-portal's restore-token-specific `InvalidArgument`.
///
/// `SelectSources` can reject several unrelated options with the same D-Bus
/// error name, so the stage and the fixed portal message both matter. Retrying
/// every `InvalidArgument` would duplicate a failure that removing the token
/// cannot possibly repair.
#[must_use]
pub fn is_restore_token_rejection(detail: &str) -> bool {
    detail.to_ascii_lowercase().contains("restore token")
}

/// Renders a source-type mask in words.
#[must_use]
pub fn describe_sources(mask: u32) -> String {
    let mut parts = Vec::new();
    if mask & source_type::MONITOR != 0 {
        parts.push("whole monitors");
    }
    if mask & source_type::WINDOW != 0 {
        parts.push("individual windows");
    }
    if mask & source_type::VIRTUAL != 0 {
        parts.push("virtual sources");
    }
    match parts.len() {
        0 => "no capture sources".into(),
        1 => parts[0].into(),
        _ => {
            let last = parts.pop().unwrap_or_default();
            format!("{} and {last}", parts.join(", "))
        }
    }
}
