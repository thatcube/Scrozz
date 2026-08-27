//! The `org.freedesktop.portal.ScreenCast` conversation.
//!
//! This is the only module that talks to D-Bus, and it is kept deliberately
//! small: it turns a [`SessionPlan`] into a PipeWire node and a socket, maps
//! every `ashpd` error onto a [`PortalFailure`], and does nothing else. The
//! decisions live in [`super::portal`], the pixels in [`super::pipewire`].
//!
//! # The sequence, and why it is this shape
//!
//! ```text
//! CreateSession      -> a session handle
//! SelectSources      -> what to offer, and the restore token
//! Start              -> the picker; stream node ids; a restore token back
//! OpenPipeWireRemote -> a socket fd for the PipeWire graph
//! ```
//!
//! `SelectSources` is where a stored restore token goes in and `Start` is where
//! a token comes back — including when the old one was accepted, because the
//! portal is entitled to rotate it. Failing to store the rotated token is a
//! silent regression to a prompt on every capture, which is exactly what the
//! persistence requirement exists to prevent, so the caller writes it back on
//! every success rather than only when it changed.
//!
//! # Blocking
//!
//! The portal API is async and Scrozz's capture path is not. With `ashpd` built
//! on `async-io` rather than tokio, zbus drives its own executor on an internal
//! thread, so blocking this thread on the future is safe and needs no runtime.
//!
//! # GNOME, honestly
//!
//! On GNOME the picker is `gnome-shell` itself. Scrozz does not, and must not,
//! draw a window list or an overlay of its own: it has no way to know what the
//! windows are — there is no protocol — and a fabricated picker would be a list
//! of guesses whose entries fail at capture time. The user chooses in the
//! Shell's dialog, and the restore token is what stops that dialog appearing
//! again.

use std::os::fd::OwnedFd;

use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode, ResponseError};
use ashpd::enumflags2::BitFlags;

use super::portal::{
    PortalFailure, SessionPlan, StreamInfo, cursor_mode, persist_mode, source_type,
};

/// Everything a successful negotiation yields.
#[derive(Debug)]
pub struct Negotiation {
    /// The streams the portal granted, in the order it reported them.
    pub streams: Vec<StreamInfo>,
    /// The token to present next time, when the portal issued one.
    pub restore_token: Option<String>,
    /// The PipeWire socket. Consumed by [`super::pipewire::acquire_frame`].
    pub remote: OwnedFd,
}

/// Runs the whole ScreenCast conversation, blocking until it settles.
///
/// `restore_token` is the token stored from a previous capture, if any. A token
/// the portal rejects is not fatal: the caller is expected to forget it and try
/// once more without one, which costs a prompt rather than a failure.
///
/// # Errors
///
/// A [`PortalFailure`], which [`PortalFailure::into_error`] turns into something
/// the user can act on. User cancellation arrives as [`PortalFailure::Cancelled`]
/// and must be propagated rather than reported.
pub fn negotiate(
    plan: &SessionPlan,
    restore_token: Option<&str>,
) -> Result<Negotiation, PortalFailure> {
    futures_lite::future::block_on(negotiate_async(plan, restore_token))
}

async fn negotiate_async(
    plan: &SessionPlan,
    restore_token: Option<&str>,
) -> Result<Negotiation, PortalFailure> {
    let proxy = Screencast::new().await.map_err(classify)?;

    // D8: ask what the portal can do rather than assume. A request naming a
    // source type the backend does not advertise is rejected outright, so
    // narrowing first is the difference between "this compositor's portal has no
    // window capture" and "screen capture failed".
    let available_types = proxy.available_source_types().await.map_err(classify)?;
    let available_cursors = proxy.available_cursor_modes().await.map_err(classify)?;
    let available_mask = source_mask(available_types);

    let narrowed = plan
        .clone()
        .narrow(available_mask, cursor_mask(available_cursors))
        .ok_or(PortalFailure::NoSources {
            wanted: plan.types,
            available: available_mask,
        })?;

    let session = proxy
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(classify)?;

    let options = SelectSourcesOptions::default()
        .set_multiple(narrowed.multiple)
        .set_cursor_mode(to_cursor_mode(narrowed.cursor))
        .set_sources(to_source_types(narrowed.types))
        .set_persist_mode(to_persist_mode(narrowed.persist))
        .set_restore_token(restore_token);

    proxy
        .select_sources(&session, options)
        .await
        .map_err(classify)?
        .response()
        .map_err(classify)?;

    // No parent window identifier: capture is triggered from a hotkey or the
    // tray, and there is no Scrozz window for the dialog to be modal to. Passing
    // a stale identifier parks the picker behind whatever was last focused.
    let response = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .map_err(classify)?
        .response()
        .map_err(classify)?;

    let streams: Vec<StreamInfo> = response
        .streams()
        .iter()
        .map(|stream| StreamInfo {
            node_id: stream.pipe_wire_node_id(),
            position: stream.position(),
            size: stream.size(),
        })
        .collect();

    if streams.is_empty() {
        return Err(PortalFailure::NoStreams);
    }

    let remote = proxy
        .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
        .await
        .map_err(classify)?;

    Ok(Negotiation {
        streams,
        restore_token: response.restore_token().map(ToOwned::to_owned),
        remote,
    })
}

/// Maps an `ashpd` error onto the classification the rest of the code reasons
/// about.
///
/// The cancellation cases are the point of this function. A dismissed dialog
/// reaches the client two different ways depending on whether the portal
/// answered with a response code or a D-Bus error, and both mean the same thing
/// to the person who dismissed it: they changed their mind. Anything that lets
/// one of those become an error dialog has made the tool worse.
fn classify(error: ashpd::Error) -> PortalFailure {
    use ashpd::{Error as Ashpd, PortalError};

    match error {
        Ashpd::Response(ResponseError::Cancelled) | Ashpd::Portal(PortalError::Cancelled(_)) => {
            PortalFailure::Cancelled
        }
        Ashpd::PortalNotFound(name) => {
            PortalFailure::Missing(format!("nothing on the bus implements {name}"))
        }
        Ashpd::RequiresVersion(required, available) => PortalFailure::Missing(format!(
            "the installed portal implements ScreenCast version {available}, but version \
             {required} is needed"
        )),
        Ashpd::Portal(PortalError::NotAllowed(why)) => PortalFailure::Bus(format!(
            "the portal refused the request: {why}. On a managed or kiosk session the screen-cast \
             permission can be disabled by policy"
        )),
        other => PortalFailure::Bus(other.to_string()),
    }
}

/// Collapses `ashpd`'s flag set back to the plain mask the plan reasons about.
fn source_mask(types: BitFlags<SourceType>) -> u32 {
    let mut mask = 0;
    if types.contains(SourceType::Monitor) {
        mask |= source_type::MONITOR;
    }
    if types.contains(SourceType::Window) {
        mask |= source_type::WINDOW;
    }
    if types.contains(SourceType::Virtual) {
        mask |= source_type::VIRTUAL;
    }
    mask
}

/// Collapses `ashpd`'s cursor flag set to a plain mask.
fn cursor_mask(modes: BitFlags<CursorMode>) -> u32 {
    let mut mask = 0;
    if modes.contains(CursorMode::Hidden) {
        mask |= cursor_mode::HIDDEN;
    }
    if modes.contains(CursorMode::Embedded) {
        mask |= cursor_mode::EMBEDDED;
    }
    if modes.contains(CursorMode::Metadata) {
        mask |= cursor_mode::METADATA;
    }
    mask
}

fn to_source_types(mask: u32) -> BitFlags<SourceType> {
    let mut flags = BitFlags::empty();
    if mask & source_type::MONITOR != 0 {
        flags |= SourceType::Monitor;
    }
    if mask & source_type::WINDOW != 0 {
        flags |= SourceType::Window;
    }
    if mask & source_type::VIRTUAL != 0 {
        flags |= SourceType::Virtual;
    }
    flags
}

/// The plan holds exactly one cursor bit by construction; `Hidden` is the
/// universal fallback because every portal backend implements it.
fn to_cursor_mode(mask: u32) -> CursorMode {
    if mask & cursor_mode::EMBEDDED != 0 {
        CursorMode::Embedded
    } else if mask & cursor_mode::METADATA != 0 {
        CursorMode::Metadata
    } else {
        CursorMode::Hidden
    }
}

fn to_persist_mode(mask: u32) -> PersistMode {
    match mask {
        persist_mode::APPLICATION => PersistMode::Application,
        persist_mode::EXPLICITLY_REVOKED => PersistMode::ExplicitlyRevoked,
        _ => PersistMode::DoNot,
    }
}
