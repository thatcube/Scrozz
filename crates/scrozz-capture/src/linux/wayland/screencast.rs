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
//! caller persists the Start token, then validates the selected target
//! OpenPipeWireRemote -> a socket fd for the PipeWire graph
//! caller copies frame
//! Session.Close      -> compositor and portal resources are released
//! ```
//!
//! `SelectSources` is where a stored restore token goes in and `Start` is where
//! a token comes back — including when the old one was accepted, because the
//! portal is entitled to rotate it. Failing to store the rotated token is a
//! silent regression to a prompt on every capture, which is exactly what the
//! persistence requirement exists to prevent, so the caller writes it back
//! immediately after `Start`, before fallible target validation or PipeWire
//! work. A target mismatch then invalidates that freshly written token before
//! returning or retrying.
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

use std::collections::HashMap;
use std::future::Future;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode, ResponseError, Session};
use ashpd::enumflags2::BitFlags;
use ashpd::zbus::zvariant::{OwnedObjectPath, OwnedValue, Structure, Value};
use futures_lite::StreamExt;

use super::portal::{
    PortalFailure, SessionPlan, StreamInfo, cursor_mode, is_restore_token_rejection, persist_mode,
    source_type,
};
use crate::CaptureCancellation;

const CONTROL_CALL_TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const PORTAL_DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST_INTERFACE: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

static HANDLE_TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Everything a successful negotiation yields.
#[derive(Debug)]
pub struct Negotiation {
    /// The streams the portal granted, in the order it reported them.
    pub streams: Vec<StreamInfo>,
    /// The token to present next time, when the portal issued one.
    pub restore_token: Option<String>,
    /// ScreenCast interface version that defines the required stream properties.
    pub(crate) portal_version: u32,
    /// A successful `Start` whose result dictionary was malformed.
    ///
    /// Keeping the successful session and any decodable restore-token rotation
    /// lets the caller commit then invalidate that token before reporting the
    /// protocol failure.
    pub(crate) start_error: Option<String>,
    /// Portal proxy retained until the PipeWire remote has been opened.
    proxy: Screencast,
    /// Dedicated connection whose unique sender owns this negotiation's portal
    /// requests and session.
    connection: Option<ashpd::zbus::Connection>,
    /// The PipeWire socket, opened only after the caller validates and persists
    /// the `Start` result.
    remote: Option<OwnedFd>,
    /// Kept alive until the frame is copied, then explicitly closed.
    ///
    /// Dropping ashpd's proxy does not call `org.freedesktop.portal.Session.Close`;
    /// without this handle, every capture would leak a compositor session until
    /// the D-Bus connection or process ended.
    session: Option<Session<Screencast>>,
}

struct StartResult {
    streams: Vec<StreamInfo>,
    restore_token: Option<String>,
    error: Option<String>,
}

impl StartResult {
    fn invalid(restore_token: Option<String>, error: impl Into<String>) -> Self {
        Self {
            streams: Vec::new(),
            restore_token,
            error: Some(error.into()),
        }
    }
}

impl Negotiation {
    /// Opens the PipeWire remote after the caller has committed the restore token.
    ///
    /// Keeping this separate from `Start` prevents a later remote failure from
    /// losing a token the portal has already issued or rotated.
    pub fn open_remote(
        &mut self,
        cancellation: Option<&CaptureCancellation>,
    ) -> Result<&OwnedFd, PortalFailure> {
        if self.remote.is_none() {
            let session = self.session.as_ref().ok_or_else(|| {
                PortalFailure::Bus(
                    "the ScreenCast session closed before its PipeWire remote was opened".into(),
                )
            })?;
            let result = futures_lite::future::block_on(await_control_call(
                self.proxy
                    .open_pipe_wire_remote(session, OpenPipeWireRemoteOptions::default()),
                cancellation,
                "OpenPipeWireRemote",
            ));
            let remote = match result {
                Ok(remote) => remote.map_err(classify)?,
                Err(PortalFailure::Cancelled) => {
                    self.close_cancelled();
                    return Err(PortalFailure::Cancelled);
                }
                Err(other) => return Err(other),
            };
            self.remote = Some(remote);
        }
        self.remote.as_ref().ok_or_else(|| {
            PortalFailure::Bus("the portal returned no PipeWire remote after opening it".into())
        })
    }

    /// Closes the portal session after its PipeWire frame has been copied.
    ///
    /// # Errors
    ///
    /// Returns the classified D-Bus failure so the caller can report cleanup
    /// trouble without hiding the capture's primary result.
    pub fn close(&mut self) -> Result<(), PortalFailure> {
        let session_result = if let Some(session) = self.session.take() {
            tracing::debug!("closing the desktop-portal ScreenCast session");
            match futures_lite::future::block_on(await_with_timeout(
                session.close(),
                "Session.Close",
                SESSION_CLOSE_TIMEOUT,
            )) {
                Ok(result) => result.map_err(classify),
                Err(error) => Err(error),
            }
        } else {
            Ok(())
        };

        let connection_result = self.disconnect();
        match (session_result, connection_result) {
            (Ok(()), Ok(())) => {
                tracing::debug!("desktop-portal ScreenCast session closed");
                Ok(())
            }
            (Err(session_error), Ok(())) => {
                tracing::warn!(
                    ?session_error,
                    "Session.Close failed; the dedicated D-Bus connection was closed instead"
                );
                Ok(())
            }
            (Ok(()), Err(connection_error)) => Err(connection_error),
            (Err(session_error), Err(connection_error)) => Err(PortalFailure::Bus(format!(
                "Session.Close failed ({session_error}); closing its dedicated D-Bus connection \
                 also failed ({connection_error})"
            ))),
        }
    }

    fn close_cancelled(&mut self) {
        // Disconnect first instead of waiting for another portal round trip.
        // Session objects are scoped to this connection's unique sender, so this
        // also revokes the known session and every late request outcome.
        self.session.take();
        if let Err(close_error) = self.disconnect() {
            tracing::warn!(
                ?close_error,
                "could not close the cancelled desktop-portal ScreenCast session"
            );
        }
    }

    fn disconnect(&mut self) -> Result<(), PortalFailure> {
        let Some(connection) = self.connection.take() else {
            return Ok(());
        };
        futures_lite::future::block_on(disconnect_connection(connection))
    }
}

impl Drop for Negotiation {
    fn drop(&mut self) {
        if let Err(close_error) = self.close() {
            tracing::warn!(
                ?close_error,
                "could not close the desktop-portal ScreenCast session during teardown"
            );
        }
    }
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
    cancellation: Option<&CaptureCancellation>,
) -> Result<Negotiation, PortalFailure> {
    futures_lite::future::block_on(negotiate_async(plan, restore_token, cancellation))
}

async fn negotiate_async(
    plan: &SessionPlan,
    restore_token: Option<&str>,
    cancellation: Option<&CaptureCancellation>,
) -> Result<Negotiation, PortalFailure> {
    // Use a dedicated bus connection for the entire negotiation. ashpd keeps
    // CreateSession's predicted request/session handles private until the call
    // completes, so cancellation or timeout cannot close them directly. Dropping
    // this connection deterministically revokes every portal object owned by its
    // unique D-Bus sender instead of orphaning a late CreateSession result on
    // ashpd's process-global connection.
    let connection = await_control_call(
        ashpd::zbus::Connection::session(),
        cancellation,
        "opening a dedicated portal D-Bus connection",
    )
    .await?
    .map_err(|error| {
        PortalFailure::Bus(format!(
            "could not open a dedicated D-Bus connection for ScreenCast: {error}"
        ))
    })?;
    let proxy = match await_control_call(
        Screencast::with_connection(connection.clone()),
        cancellation,
        "constructing ScreenCast proxy",
    )
    .await
    {
        Ok(Ok(proxy)) => proxy,
        Ok(Err(error)) => {
            let failure = classify(error);
            disconnect_after_failure(connection).await;
            return Err(failure);
        }
        Err(error) => {
            disconnect_after_failure(connection).await;
            return Err(error);
        }
    };

    // D8: ask what the portal can do rather than assume. A request naming a
    // source type the backend does not advertise is rejected outright, so
    // narrowing first is the difference between "this compositor's portal has no
    // window capture" and "screen capture failed".
    let available_types = match await_control_call(
        proxy.available_source_types(),
        cancellation,
        "reading AvailableSourceTypes",
    )
    .await
    {
        Ok(Ok(types)) => types,
        Ok(Err(error)) => {
            let failure = classify(error);
            disconnect_after_failure(connection).await;
            return Err(failure);
        }
        Err(error) => {
            disconnect_after_failure(connection).await;
            return Err(error);
        }
    };
    let available_cursors = match await_control_call(
        proxy.available_cursor_modes(),
        cancellation,
        "reading AvailableCursorModes",
    )
    .await
    {
        Ok(Ok(cursors)) => cursors,
        Ok(Err(error)) => {
            let failure = classify(error);
            disconnect_after_failure(connection).await;
            return Err(failure);
        }
        Err(error) => {
            disconnect_after_failure(connection).await;
            return Err(error);
        }
    };
    let available_mask = source_mask(available_types);

    let Some(narrowed) = plan
        .clone()
        .narrow(available_mask, cursor_mask(available_cursors))
    else {
        let failure = PortalFailure::NoSources {
            wanted: plan.types,
            available: available_mask,
        };
        disconnect_after_failure(connection).await;
        return Err(failure);
    };

    // This call can now be raced safely: if ashpd has not exposed the session
    // handle when cancellation or timeout wins, returning drops the dedicated
    // connection and the portal revokes any late-created session for that sender.
    let session = match await_control_call(
        proxy.create_session(CreateSessionOptions::default()),
        cancellation,
        "CreateSession",
    )
    .await
    {
        Ok(Ok(session)) => session,
        Ok(Err(error)) => {
            let failure = classify(error);
            disconnect_after_failure(connection).await;
            return Err(failure);
        }
        Err(error) => {
            disconnect_after_failure(connection).await;
            return Err(error);
        }
    };

    let result = complete_session(
        &connection,
        &proxy,
        &session,
        narrowed,
        restore_token,
        cancellation,
    )
    .await;
    match result {
        Ok(started) => Ok(Negotiation {
            streams: started.streams,
            restore_token: started.restore_token,
            portal_version: proxy.version(),
            start_error: started.error,
            proxy,
            connection: Some(connection),
            remote: None,
            session: Some(session),
        }),
        Err(PortalFailure::Cancelled) => {
            // Do not wait for Session.Close after cancellation. Closing the
            // dedicated connection immediately revokes both the session and any
            // portal request whose late response ashpd has suppressed.
            disconnect_after_failure(connection).await;
            Err(PortalFailure::Cancelled)
        }
        Err(failure) => {
            if let Err(close_error) = close_session(&session).await {
                tracing::warn!(
                    ?close_error,
                    ?failure,
                    "could not close a failed desktop-portal ScreenCast session"
                );
            }
            disconnect_after_failure(connection).await;
            Err(failure)
        }
    }
}

async fn disconnect_connection(connection: ashpd::zbus::Connection) -> Result<(), PortalFailure> {
    await_with_timeout(
        connection.close(),
        "closing the dedicated portal D-Bus connection",
        SESSION_CLOSE_TIMEOUT,
    )
    .await?
    .map_err(|error| {
        PortalFailure::Bus(format!(
            "could not close the dedicated portal D-Bus connection: {error}"
        ))
    })
}

async fn disconnect_after_failure(connection: ashpd::zbus::Connection) {
    if let Err(close_error) = disconnect_connection(connection).await {
        tracing::warn!(
            ?close_error,
            "could not close the failed desktop-portal negotiation connection"
        );
    }
}

async fn await_or_cancel<F>(
    future: F,
    cancellation: Option<&CaptureCancellation>,
) -> Result<F::Output, PortalFailure>
where
    F: Future,
{
    let Some(cancellation) = cancellation else {
        return Ok(future.await);
    };

    futures_lite::future::race(async { Ok(future.await) }, async {
        cancellation.cancelled().await;
        Err(PortalFailure::Cancelled)
    })
    .await
}

async fn await_with_timeout<F>(
    future: F,
    operation: &'static str,
    timeout: Duration,
) -> Result<F::Output, PortalFailure>
where
    F: Future,
{
    futures_lite::future::race(async { Ok(future.await) }, async move {
        async_io::Timer::after(timeout).await;
        Err(PortalFailure::Bus(format!(
            "{operation} did not answer within {} seconds",
            timeout.as_secs()
        )))
    })
    .await
}

async fn await_control_call<F>(
    future: F,
    cancellation: Option<&CaptureCancellation>,
    operation: &'static str,
) -> Result<F::Output, PortalFailure>
where
    F: Future,
{
    await_or_cancel(
        await_with_timeout(future, operation, CONTROL_CALL_TIMEOUT),
        cancellation,
    )
    .await?
}

async fn close_session(session: &Session<Screencast>) -> Result<(), PortalFailure> {
    await_with_timeout(session.close(), "Session.Close", SESSION_CLOSE_TIMEOUT)
        .await?
        .map_err(classify)
}

async fn complete_session(
    connection: &ashpd::zbus::Connection,
    proxy: &Screencast,
    session: &Session<Screencast>,
    narrowed: SessionPlan,
    restore_token: Option<&str>,
    cancellation: Option<&CaptureCancellation>,
) -> Result<StartResult, PortalFailure> {
    tracing::debug!(
        sources = narrowed.types,
        cursor = narrowed.cursor,
        restore_token = restore_token.is_some(),
        "selecting desktop-portal ScreenCast sources"
    );
    let options = SelectSourcesOptions::default()
        .set_multiple(false)
        .set_cursor_mode(to_cursor_mode(narrowed.cursor))
        .set_sources(to_source_types(narrowed.types))
        .set_persist_mode(to_persist_mode(narrowed.persist))
        .set_restore_token(restore_token);

    await_control_call(
        proxy.select_sources(session, options),
        cancellation,
        "SelectSources",
    )
    .await?
    .map_err(|error| classify_select_sources(error, restore_token.is_some()))?
    .response()
    .map_err(|error| classify_select_sources(error, restore_token.is_some()))?;

    // ashpd 0.13 does not expose ScreenCast v6's `pipewire-serial`. Decode this
    // one response directly, while retaining the request path so cancellation
    // closes the picker rather than merely dropping the client-side future.
    let started = start(connection, session, cancellation).await?;
    tracing::debug!(
        streams = started.streams.len(),
        positioned = started
            .streams
            .iter()
            .filter(|stream| stream.position.is_some())
            .count(),
        sized = started
            .streams
            .iter()
            .filter(|stream| stream.size.is_some())
            .count(),
        restore_token = started.restore_token.is_some(),
        valid = started.error.is_none(),
        "desktop-portal ScreenCast session started"
    );

    Ok(started)
}

/// Starts an interactive portal request and decodes the v6 stream dictionary.
async fn start(
    connection: &ashpd::zbus::Connection,
    session: &Session<Screencast>,
    cancellation: Option<&CaptureCancellation>,
) -> Result<StartResult, PortalFailure> {
    let handle_token = format!(
        "scrozz_{}_{}",
        std::process::id(),
        HANDLE_TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let unique_name = connection.unique_name().ok_or_else(|| {
        PortalFailure::Bus("the dedicated portal connection has no unique D-Bus name".into())
    })?;
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    let request_path = OwnedObjectPath::try_from(format!(
        "/org/freedesktop/portal/desktop/request/{sender}/{handle_token}"
    ))
    .map_err(|error| {
        PortalFailure::Bus(format!(
            "could not construct the ScreenCast Start request path: {error}"
        ))
    })?;

    let request_proxy = await_control_call(
        ashpd::zbus::Proxy::new(
            connection,
            PORTAL_DESTINATION,
            request_path.clone(),
            REQUEST_INTERFACE,
        ),
        cancellation,
        "constructing the ScreenCast Start request",
    )
    .await?
    .map_err(classify_zbus)?;
    let mut responses = await_control_call(
        request_proxy.receive_signal("Response"),
        cancellation,
        "subscribing to the ScreenCast Start response",
    )
    .await?
    .map_err(classify_zbus)?;
    let screencast_proxy = await_control_call(
        ashpd::zbus::Proxy::new(
            connection,
            PORTAL_DESTINATION,
            PORTAL_PATH,
            SCREENCAST_INTERFACE,
        ),
        cancellation,
        "constructing the raw ScreenCast proxy",
    )
    .await?
    .map_err(classify_zbus)?;

    let mut options = HashMap::new();
    options.insert("handle_token", Value::from(handle_token.as_str()));
    // No parent window identifier: capture is triggered from a hotkey or the
    // tray, and there is no Scrozz window for the dialog to be modal to.
    let returned_path: OwnedObjectPath = match await_control_call(
        screencast_proxy.call("Start", &(session, "", options)),
        cancellation,
        "starting the ScreenCast picker",
    )
    .await
    {
        Ok(Ok(path)) => path,
        Ok(Err(error)) => return Err(classify_zbus(error)),
        Err(PortalFailure::Cancelled) => {
            close_request(&request_proxy).await;
            return Err(PortalFailure::Cancelled);
        }
        Err(error) => {
            close_request(&request_proxy).await;
            return Err(error);
        }
    };
    if returned_path != request_path {
        close_request_path(connection, &returned_path).await;
        close_request(&request_proxy).await;
        return Err(PortalFailure::Bus(format!(
            "the ScreenCast portal returned unexpected Start request path {returned_path}; \
             expected {request_path}"
        )));
    }

    let response = match await_or_cancel(responses.next(), cancellation).await {
        Ok(Some(message)) => message,
        Ok(None) => {
            return Err(PortalFailure::Bus(
                "the ScreenCast Start request ended without a Response signal".into(),
            ));
        }
        Err(PortalFailure::Cancelled) => {
            close_request(&request_proxy).await;
            return Err(PortalFailure::Cancelled);
        }
        Err(error) => return Err(error),
    };
    let (response_code, mut results): (u32, HashMap<String, OwnedValue>) =
        response.body().deserialize().map_err(|error| {
            PortalFailure::Bus(format!(
                "could not decode the ScreenCast Start response: {error}"
            ))
        })?;
    match response_code {
        0 => {}
        1 => return Err(PortalFailure::Cancelled),
        _ => {
            return Err(classify(ashpd::Error::Response(ResponseError::Other)));
        }
    }

    // Decode the rotation first. If any later stream field is malformed, the
    // caller can still durably replace and then invalidate a single-use token
    // consumed by this successful Start.
    let restore_token = match results
        .remove("restore_token")
        .map(String::try_from)
        .transpose()
    {
        Ok(token) => token,
        Err(error) => {
            return Ok(StartResult::invalid(
                None,
                format!("could not decode the ScreenCast restore token: {error}"),
            ));
        }
    };
    let Some(streams_value) = results.remove("streams") else {
        return Ok(StartResult::invalid(
            restore_token,
            "the successful ScreenCast Start response omitted its streams property",
        ));
    };
    let raw_streams: Vec<(u32, HashMap<String, OwnedValue>)> = match streams_value.try_into() {
        Ok(streams) => streams,
        Err(error) => {
            return Ok(StartResult::invalid(
                restore_token,
                format!("could not decode the ScreenCast Start stream list: {error}"),
            ));
        }
    };
    let mut streams = Vec::with_capacity(raw_streams.len());
    for (node_id, mut properties) in raw_streams {
        let pipewire_serial = match take_u64(&mut properties, "pipewire-serial") {
            Ok(value) => value,
            Err(error) => return Ok(StartResult::invalid(restore_token, error)),
        };
        let source_type = match take_u32(&mut properties, "source_type") {
            Ok(value) => value,
            Err(error) => return Ok(StartResult::invalid(restore_token, error)),
        };
        let position = match take_i32_pair(&mut properties, "position") {
            Ok(value) => value,
            Err(error) => return Ok(StartResult::invalid(restore_token, error)),
        };
        let size = match take_i32_pair(&mut properties, "size") {
            Ok(value) => value,
            Err(error) => return Ok(StartResult::invalid(restore_token, error)),
        };
        let stream = StreamInfo {
            node_id,
            pipewire_serial,
            source_type,
            position,
            size,
        };
        streams.push(stream);
    }

    Ok(StartResult {
        streams,
        restore_token,
        error: None,
    })
}

fn take_u32(
    properties: &mut HashMap<String, OwnedValue>,
    key: &str,
) -> Result<Option<u32>, String> {
    properties
        .remove(key)
        .map(u32::try_from)
        .transpose()
        .map_err(|error| format!("could not decode ScreenCast stream property `{key}`: {error}"))
}

fn take_u64(
    properties: &mut HashMap<String, OwnedValue>,
    key: &str,
) -> Result<Option<u64>, String> {
    properties
        .remove(key)
        .map(u64::try_from)
        .transpose()
        .map_err(|error| format!("could not decode ScreenCast stream property `{key}`: {error}"))
}

fn take_i32_pair(
    properties: &mut HashMap<String, OwnedValue>,
    key: &str,
) -> Result<Option<(i32, i32)>, String> {
    properties
        .remove(key)
        .map(|value| {
            let structure = Structure::try_from(value)?;
            <(i32, i32)>::try_from(structure)
        })
        .transpose()
        .map_err(|error| format!("could not decode ScreenCast stream property `{key}`: {error}"))
}

async fn close_request(request: &ashpd::zbus::Proxy<'_>) {
    match await_with_timeout(
        request.call::<_, _, ()>("Close", &()),
        "Request.Close",
        SESSION_CLOSE_TIMEOUT,
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "could not close the cancelled ScreenCast request");
        }
        Err(error) => {
            tracing::warn!(%error, "timed out closing the cancelled ScreenCast request");
        }
    }
}

async fn close_request_path(connection: &ashpd::zbus::Connection, request_path: &OwnedObjectPath) {
    let proxy = ashpd::zbus::Proxy::new(
        connection,
        PORTAL_DESTINATION,
        request_path.clone(),
        REQUEST_INTERFACE,
    )
    .await;
    match proxy {
        Ok(proxy) => close_request(&proxy).await,
        Err(error) => {
            tracing::warn!(%error, %request_path, "could not address an unexpected portal request");
        }
    }
}

fn classify_zbus(error: ashpd::zbus::Error) -> PortalFailure {
    if let ashpd::zbus::Error::MethodError(name, detail, _) = &error {
        let name = name.as_str();
        let detail = detail.clone().unwrap_or_else(|| name.to_owned());
        if name.ends_with(".Cancelled") {
            return PortalFailure::Cancelled;
        }
        if name.ends_with(".NotAllowed") {
            return PortalFailure::PermissionDenied(detail);
        }
        if name.ends_with(".NotFound") {
            return PortalFailure::Missing(detail);
        }
    }
    PortalFailure::Bus(error.to_string())
}

/// Classifies only the token-specific `SelectSources` failure as retryable.
///
/// `InvalidArgument` also covers source and cursor options. Retrying every one
/// of those after deleting a token would duplicate an unrelated failure.
fn classify_select_sources(error: ashpd::Error, used_restore_token: bool) -> PortalFailure {
    use ashpd::{Error as Ashpd, PortalError};

    match error {
        Ashpd::Portal(PortalError::InvalidArgument(detail))
            if used_restore_token && is_restore_token_rejection(&detail) =>
        {
            PortalFailure::RestoreRejected(detail)
        }
        other => classify(other),
    }
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
            tracing::debug!("desktop-portal ScreenCast picker was cancelled");
            PortalFailure::Cancelled
        }
        Ashpd::PortalNotFound(name) => {
            PortalFailure::Missing(format!("nothing on the bus implements {name}"))
        }
        Ashpd::RequiresVersion(required, available) => PortalFailure::Missing(format!(
            "the installed portal implements ScreenCast version {available}, but version \
             {required} is needed"
        )),
        Ashpd::Portal(PortalError::NotAllowed(why)) => PortalFailure::PermissionDenied(why),
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
