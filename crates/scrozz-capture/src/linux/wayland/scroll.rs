//! Wayland scroll input through the RemoteDesktop portal.
//!
//! `ashpd` is asynchronous and a portal session must remain alive between
//! gestures, while the core driver contract is synchronous. A dedicated worker
//! thread owns both a Tokio runtime and the session; callers exchange typed
//! commands over channels, so there is no nested-runtime panic and no borrowed
//! async state escaping the platform boundary.

use std::{
    sync::mpsc,
    thread::{self, JoinHandle},
};

use ashpd::{
    PortalError,
    desktop::{
        CreateSessionOptions, PersistMode, ResponseError, Session,
        remote_desktop::{
            DeviceType, NotifyPointerAxisOptions, NotifyPointerMotionAbsoluteOptions,
            RemoteDesktop, SelectDevicesOptions, StartOptions,
        },
        screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    },
};
use scrozz_core::{
    Error, ManualScrollDriver, Result, ScrollCapabilities, ScrollDriver, ScrollGesture,
};
use tokio::runtime::Builder;

use crate::scroll_units::{self, PortalStreamGeometry};

use super::super::session::{self, Compositor, SessionEnv};

pub(crate) fn driver_for_session(env: &SessionEnv) -> Box<dyn ScrollDriver> {
    let compositor = session::detect_compositor(env);
    if !session::capabilities(&compositor).remote_desktop {
        return Box::new(ManualScrollDriver::new(manual_reason(&compositor)));
    }

    match PortalScrollDriver::connect(compositor.clone()) {
        Ok(driver) => Box::new(driver),
        Err(reason) => Box::new(ManualScrollDriver::new(format!(
            "{reason}; start or update xdg-desktop-portal and its {compositor} backend, or scroll \
             manually while Scrozz follows"
        ))),
    }
}

fn manual_reason(compositor: &Compositor) -> String {
    match compositor {
        Compositor::Wlroots => "xdg-desktop-portal-wlr does not implement the RemoteDesktop \
                                portal, so Wayland will not let Scrozz send wheel input — scroll \
                                manually while Scrozz follows"
            .into(),
        _ => format!(
            "{compositor} is not known to provide a usable RemoteDesktop portal, and Wayland has \
             no direct input-synthesis protocol — scroll manually while Scrozz follows"
        ),
    }
}

struct PortalScrollDriver {
    commands: mpsc::Sender<Command>,
    worker: Option<JoinHandle<()>>,
    name: String,
}

impl std::fmt::Debug for PortalScrollDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalScrollDriver")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl PortalScrollDriver {
    fn connect(compositor: Compositor) -> std::result::Result<Self, String> {
        let (commands, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread_name = format!("scrozz-portal-scroll-{compositor}");
        let worker = thread::Builder::new()
            .name(thread_name)
            .spawn(move || worker_main(receiver, ready_tx))
            .map_err(|error| format!("could not start the RemoteDesktop worker: {error}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                worker: Some(worker),
                name: format!("xdg-desktop-portal RemoteDesktop on {compositor}"),
            }),
            Ok(Err(reason)) => {
                let _ = worker.join();
                Err(reason)
            }
            Err(error) => {
                let _ = worker.join();
                Err(format!(
                    "the RemoteDesktop worker exited before probing the portal: {error}"
                ))
            }
        }
    }

    fn request(&self, command: impl FnOnce(mpsc::SyncSender<Result<()>>) -> Command) -> Result<()> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.commands
            .send(command(reply_tx))
            .map_err(|_| Error::Platform("the RemoteDesktop worker has stopped".into()))?;
        reply_rx
            .recv()
            .map_err(|_| Error::Platform("the RemoteDesktop worker returned no result".into()))?
    }
}

impl ScrollDriver for PortalScrollDriver {
    fn capabilities(&self) -> ScrollCapabilities {
        // `connect` has already reached both portal interfaces and queried the
        // pointer/monitor capabilities. It is therefore safe to advertise this.
        ScrollCapabilities::automatic(true)
    }

    fn prepare(&mut self) -> Result<()> {
        self.request(Command::Prepare)
    }

    fn scroll(&mut self, gesture: &ScrollGesture) -> Result<()> {
        if gesture.is_noop() {
            return Ok(());
        }
        let gesture = *gesture;
        self.request(|reply| Command::Scroll { gesture, reply })
    }

    fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for PortalScrollDriver {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum Command {
    Prepare(mpsc::SyncSender<Result<()>>),
    Scroll {
        gesture: ScrollGesture,
        reply: mpsc::SyncSender<Result<()>>,
    },
    Shutdown,
}

fn worker_main(
    commands: mpsc::Receiver<Command>,
    ready: mpsc::SyncSender<std::result::Result<(), String>>,
) {
    let runtime = match Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(format!(
                "could not create the RemoteDesktop runtime: {error}"
            )));
            return;
        }
    };
    let mut portal = match runtime.block_on(PortalState::probe()) {
        Ok(portal) => {
            let _ = ready.send(Ok(()));
            portal
        }
        Err(reason) => {
            let _ = ready.send(Err(reason));
            return;
        }
    };

    while let Ok(command) = commands.recv() {
        match command {
            Command::Prepare(reply) => {
                let _ = reply.send(runtime.block_on(portal.prepare()));
            }
            Command::Scroll { gesture, reply } => {
                let _ = reply.send(runtime.block_on(portal.scroll(gesture)));
            }
            Command::Shutdown => break,
        }
    }
    runtime.block_on(portal.close());
}

struct PortalState {
    remote: RemoteDesktop,
    screencast: Screencast,
    session: Option<Session<RemoteDesktop>>,
    streams: Vec<PortalStreamGeometry>,
}

impl PortalState {
    async fn probe() -> std::result::Result<Self, String> {
        let remote = RemoteDesktop::new()
            .await
            .map_err(|error| format!("the RemoteDesktop portal could not be reached: {error}"))?;
        let devices = remote.available_device_types().await.map_err(|error| {
            format!("the RemoteDesktop portal did not report its device types: {error}")
        })?;
        if !devices.contains(DeviceType::Pointer) {
            return Err(
                "the RemoteDesktop portal is present but does not offer pointer input".into(),
            );
        }

        let screencast = Screencast::new()
            .await
            .map_err(|error| format!("the ScreenCast portal could not be reached: {error}"))?;
        let sources = screencast.available_source_types().await.map_err(|error| {
            format!("the ScreenCast portal did not report its source types: {error}")
        })?;
        if !sources.contains(SourceType::Monitor) {
            return Err(
                "the ScreenCast portal cannot authorize a monitor for absolute pointer targeting"
                    .into(),
            );
        }

        Ok(Self {
            remote,
            screencast,
            session: None,
            streams: Vec::new(),
        })
    }

    async fn prepare(&mut self) -> Result<()> {
        if self.session.is_some() {
            return Ok(());
        }

        let session = self
            .remote
            .create_session(CreateSessionOptions::default())
            .await
            .map_err(|error| map_portal_error(error, "creating a RemoteDesktop session"))?;
        let result = self.prepare_session(&session).await;
        match result {
            Ok(streams) => {
                self.streams = streams;
                self.session = Some(session);
                Ok(())
            }
            Err(error) => {
                if let Err(close_error) = session.close().await {
                    tracing::debug!(%close_error, "failed to close rejected RemoteDesktop session");
                }
                Err(error)
            }
        }
    }

    async fn prepare_session(
        &self,
        session: &Session<RemoteDesktop>,
    ) -> Result<Vec<PortalStreamGeometry>> {
        let request = self
            .remote
            .select_devices(
                session,
                SelectDevicesOptions::default()
                    .set_devices(Some(DeviceType::Pointer.into()))
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await
            .map_err(|error| map_portal_error(error, "requesting portal pointer access"))?;
        request
            .response()
            .map_err(|error| map_portal_error(error, "requesting portal pointer access"))?;

        let request = self
            .screencast
            .select_sources(
                session,
                SelectSourcesOptions::default()
                    .set_sources(Some(SourceType::Monitor.into()))
                    .set_multiple(true)
                    .set_cursor_mode(CursorMode::Hidden)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await
            .map_err(|error| map_portal_error(error, "requesting monitor targeting access"))?;
        request
            .response()
            .map_err(|error| map_portal_error(error, "requesting monitor targeting access"))?;

        let request = self
            .remote
            .start(session, None, StartOptions::default())
            .await
            .map_err(|error| map_portal_error(error, "starting the RemoteDesktop session"))?;
        let selected = request
            .response()
            .map_err(|error| map_portal_error(error, "starting the RemoteDesktop session"))?;
        if !selected.devices().contains(DeviceType::Pointer) {
            return Err(permission_denied(
                "the portal session started without pointer access",
            ));
        }

        let streams: Vec<_> = selected
            .streams()
            .iter()
            .filter_map(|stream| {
                Some(PortalStreamGeometry {
                    node_id: stream.pipe_wire_node_id(),
                    position: stream.position()?,
                    size: stream.size()?,
                })
            })
            .filter(|stream| stream.size.0 > 0 && stream.size.1 > 0)
            .collect();
        if streams.is_empty() {
            return Err(Error::Unsupported {
                what: "targeting Wayland scroll input".into(),
                why: "the portal granted pointer input but returned no monitor stream with \
                      compositor position and size; choose a monitor in the portal picker, or \
                      scroll manually while Scrozz follows"
                    .into(),
            });
        }
        Ok(streams)
    }

    async fn scroll(&mut self, gesture: ScrollGesture) -> Result<()> {
        if !scroll_units::finite_point(gesture.at) {
            return Err(Error::InvalidRequest(
                "the scroll target point must contain finite coordinates".into(),
            ));
        }
        let session = self.session.as_ref().ok_or_else(|| {
            Error::Platform(
                "the RemoteDesktop scroll driver must be prepared before it can send input".into(),
            )
        })?;
        let (node_id, x, y) = scroll_units::portal_stream_at(&self.streams, gesture.at)
            .ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "the scroll target ({}, {}) is outside the monitors authorized in the \
                     RemoteDesktop portal picker",
                    gesture.at.x, gesture.at.y
                ))
            })?;

        self.remote
            .notify_pointer_motion_absolute(
                session,
                node_id,
                x,
                y,
                NotifyPointerMotionAbsoluteOptions::default(),
            )
            .await
            .map_err(|error| map_portal_error(error, "aiming Wayland scroll input"))?;
        let (dx, dy) = scroll_units::portal_deltas(gesture.axis, gesture.amount);
        self.remote
            .notify_pointer_axis(
                session,
                dx,
                dy,
                NotifyPointerAxisOptions::default().set_finish(true),
            )
            .await
            .map_err(|error| map_portal_error(error, "sending Wayland pointer-axis input"))
    }

    async fn close(&mut self) {
        if let Some(session) = self.session.take()
            && let Err(error) = session.close().await
        {
            tracing::debug!(%error, "failed to close RemoteDesktop session");
        }
        self.streams.clear();
    }
}

fn map_portal_error(error: ashpd::Error, context: &str) -> Error {
    match error {
        ashpd::Error::Response(ResponseError::Cancelled)
        | ashpd::Error::Portal(PortalError::NotAllowed(_))
        | ashpd::Error::Portal(PortalError::Cancelled(_)) => permission_denied(context),
        error => Error::Platform(format!("{context}: {error}")),
    }
}

fn permission_denied(context: &str) -> Error {
    Error::PermissionDenied {
        capability: format!("RemoteDesktop pointer access ({context})"),
        remedy: "approve pointer control and monitor targeting in the desktop portal dialog; if \
                 the dialog was dismissed, start scrolling capture again"
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wlroots_selection_is_manual_without_touching_the_portal() {
        let env = SessionEnv {
            wayland_display: Some("wayland-1".into()),
            xdg_current_desktop: Some("sway".into()),
            ..SessionEnv::default()
        };
        let driver = driver_for_session(&env);
        let capabilities = driver.capabilities();

        assert!(!capabilities.is_automatic());
        assert!(!capabilities.requires_permission);
        let scrozz_core::ScrollSynthesis::Manual { why } = capabilities.synthesis else {
            panic!("wlroots must select manual scrolling");
        };
        assert!(why.contains("RemoteDesktop"), "{why}");
        assert!(why.contains("manually"), "{why}");
    }
}
