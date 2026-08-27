//! Non-blocking single-instance server owned by the running GUI.
//!
//! A bounded transport worker performs all socket or named-pipe I/O. The GUI
//! thread only polls a bounded channel and executes validated CLI commands.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crate::{
    cli::{Cli, Command},
    commands,
    fault::{CliError, CliResult},
    ipc::{self, Response},
    report::Reporter,
};

const REQUEST_QUEUE_DEPTH: usize = 16;
const WORKER_POLL: Duration = Duration::from_millis(10);
const REQUEST_QUEUED: u8 = 0;
const REQUEST_RUNNING: u8 = 1;
const REQUEST_CANCELLED: u8 = 2;
const REQUEST_COMPLETED: u8 = 3;

#[derive(Debug)]
struct RequestControl {
    phase: AtomicU8,
    cancelled: AtomicBool,
}

impl RequestControl {
    fn queued() -> Self {
        Self {
            phase: AtomicU8::new(REQUEST_QUEUED),
            cancelled: AtomicBool::new(false),
        }
    }

    fn try_start(&self, deadline: Instant) -> bool {
        if Instant::now() >= deadline {
            self.cancel();
            return false;
        }
        if self
            .phase
            .compare_exchange(
                REQUEST_QUEUED,
                REQUEST_RUNNING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        if self.cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            self.cancelled.store(true, Ordering::Release);
            let _ = self.phase.compare_exchange(
                REQUEST_RUNNING,
                REQUEST_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            return false;
        }
        true
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.phase.compare_exchange(
            REQUEST_QUEUED,
            REQUEST_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn complete(&self) {
        let _ = self.phase.compare_exchange(
            REQUEST_RUNNING,
            REQUEST_COMPLETED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// A validated request from another process, waiting for its answer.
pub struct Request {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    reply: SyncSender<Response>,
    deadline: Instant,
    control: Arc<RequestControl>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Request")
            .field("argv", &self.argv)
            .field("cwd", &self.cwd)
            .field("deadline", &self.deadline)
            .field("control", &self.control)
            .finish_non_exhaustive()
    }
}

impl Request {
    /// Executes the request and hands both output streams back to the worker.
    pub fn serve(self) -> Option<Command> {
        if !self.control.try_start(self.deadline) {
            tracing::debug!("discarding a cancelled or expired forwarded command");
            return None;
        }
        let (command, response) = run(&self.argv, self.cwd.as_deref());
        if self.reply.send(response).is_err() {
            self.control.cancel();
            tracing::debug!("forwarded command client disconnected before the reply");
        } else {
            self.control.complete();
        }
        command
    }
}

fn run(argv: &[String], cwd: Option<&Path>) -> (Option<Command>, Response) {
    use clap::Parser as _;

    if argv.is_empty() {
        let error = CliError::usage("the forwarded command had no arguments");
        return (
            None,
            response_from_error(
                error.exit().code(),
                Reporter::new(false, false).render_error("ipc", &error),
            ),
        );
    }

    let mut with_argv0 = Vec::with_capacity(argv.len() + 1);
    with_argv0.push("scrozz".to_owned());
    with_argv0.extend_from_slice(argv);
    let cli = match Cli::try_parse_from(&with_argv0) {
        Ok(cli) => cli,
        Err(error) => {
            return (
                None,
                Response {
                    code: 2,
                    stdout: Vec::new(),
                    stderr: error.to_string().into_bytes(),
                },
            );
        }
    };

    let command = cli.command.clone().unwrap_or(Command::Gui);
    let slug = command.slug();
    let result = match WorkingDirectory::enter(cwd) {
        Ok(_directory) => cli.validate().and_then(|()| commands::dispatch(&command)),
        Err(error) => Err(error),
    };

    let reporter = Reporter::from_global(&cli.global);
    let response = match result {
        Ok(report) => response_from_output(0, reporter.render(&slug, &report)),
        Err(error) => {
            response_from_error(error.exit().code(), reporter.render_error(&slug, &error))
        }
    };
    (Some(command), response)
}

fn response_from_output(code: u8, output: crate::report::RenderedOutput) -> Response {
    Response {
        code,
        stdout: output.stdout,
        stderr: output.stderr,
    }
}

fn response_from_error(code: u8, output: crate::report::RenderedOutput) -> Response {
    response_from_output(code, output)
}

struct WorkingDirectory(Option<PathBuf>);

impl WorkingDirectory {
    fn enter(target: Option<&Path>) -> CliResult<Self> {
        let Some(target) = target else {
            return Ok(Self(None));
        };
        let previous = std::env::current_dir().map_err(|error| {
            CliError::ipc(format!(
                "could not preserve the running instance working directory: {error}"
            ))
        })?;
        std::env::set_current_dir(target).map_err(|error| {
            CliError::ipc(format!(
                "could not enter the caller working directory {}: {error}",
                target.display()
            ))
        })?;
        Ok(Self(Some(previous)))
    }
}

impl Drop for WorkingDirectory {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take()
            && let Err(error) = std::env::set_current_dir(&previous)
        {
            tracing::error!(
                path = %previous.display(),
                "could not restore the GUI working directory: {error}"
            );
        }
    }
}

/// The listener held by a running GUI.
pub struct Server {
    path: PathBuf,
    requests: Receiver<Request>,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    pub fn bind() -> CliResult<Self> {
        Self::bind_at(ipc::endpoint())
    }

    #[cfg(unix)]
    pub fn bind_at(path: PathBuf) -> CliResult<Self> {
        use std::os::unix::{
            fs::{DirBuilderExt as _, PermissionsExt as _},
            net::UnixListener,
        };

        if let Some(parent) = path.parent() {
            let existed = parent.exists();
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(parent).map_err(|error| {
                CliError::ipc(format!(
                    "could not make {} for the instance socket: {error}",
                    parent.display()
                ))
            })?;
            verify_private_directory(parent)?;
            if !existed {
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(
                    |error| {
                        CliError::ipc(format!(
                            "could not protect the IPC directory {}: {error}",
                            parent.display()
                        ))
                    },
                )?;
            }
        }
        clear_stale(&path)?;
        let listener = UnixListener::bind(&path).map_err(|error| {
            CliError::ipc(format!("could not listen at {}: {error}", path.display()))
        })?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(
            |error| {
                CliError::ipc(format!(
                    "could not protect the instance socket {}: {error}",
                    path.display()
                ))
            },
        )?;
        listener.set_nonblocking(true).map_err(|error| {
            CliError::ipc(format!(
                "could not make the instance socket pollable: {error}"
            ))
        })?;
        spawn(path, move |requests, shutdown| {
            unix_worker(listener, &requests, &shutdown);
        })
    }

    #[cfg(windows)]
    pub fn bind_at(path: PathBuf) -> CliResult<Self> {
        let listener = ipc::windows_pipe::PipeListener::bind(&path)?;
        spawn(path, move |requests, shutdown| {
            windows_worker(listener, &requests, &shutdown);
        })
    }

    #[cfg(not(any(unix, windows)))]
    pub fn bind_at(path: PathBuf) -> CliResult<Self> {
        let (_sender, requests) = sync_channel(1);
        Ok(Self {
            path,
            requests,
            shutdown: Arc::new(AtomicBool::new(false)),
            worker: None,
        })
    }

    /// Takes one pending request without blocking.
    pub fn poll(&self) -> Option<Request> {
        match self.requests.try_recv() {
            Ok(request) => Some(request),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                tracing::warn!("the single-instance transport worker stopped");
                None
            }
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn spawn(
    path: PathBuf,
    run_worker: impl FnOnce(SyncSender<Request>, Arc<AtomicBool>) + Send + 'static,
) -> CliResult<Server> {
    let (requests_sender, requests) = sync_channel(REQUEST_QUEUE_DEPTH);
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = Arc::clone(&shutdown);
    let worker = std::thread::Builder::new()
        .name("scrozz-ipc".to_owned())
        .spawn(move || run_worker(requests_sender, worker_shutdown))
        .map_err(|error| CliError::ipc(format!("could not start the IPC worker: {error}")))?;
    tracing::debug!(path = %path.display(), "listening for forwarded commands");
    Ok(Server {
        path,
        requests,
        shutdown,
        worker: Some(worker),
    })
}

#[cfg(unix)]
fn unix_worker(
    listener: std::os::unix::net::UnixListener,
    requests: &SyncSender<Request>,
    shutdown: &Arc<AtomicBool>,
) {
    use std::io::ErrorKind;

    while !shutdown.load(Ordering::Acquire) {
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(WORKER_POLL);
                continue;
            }
            Err(error) => {
                tracing::warn!("could not accept a forwarded command: {error}");
                std::thread::sleep(WORKER_POLL);
                continue;
            }
        };
        if let Err(error) = ipc::configure_unix_stream(&stream) {
            tracing::warn!("{error}");
            continue;
        }
        serve_connection(&mut stream, requests, shutdown);
    }
}

#[cfg(windows)]
fn windows_worker(
    mut listener: ipc::windows_pipe::PipeListener,
    requests: &SyncSender<Request>,
    shutdown: &Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Acquire) {
        if let Err(error) = listener.accept(shutdown) {
            if !shutdown.load(Ordering::Acquire) {
                tracing::warn!("{error}");
            }
            continue;
        }
        serve_connection(&mut listener, requests, shutdown);
        listener.disconnect();
    }
}

fn serve_connection(
    stream: &mut (impl std::io::Read + std::io::Write),
    requests: &SyncSender<Request>,
    shutdown: &Arc<AtomicBool>,
) {
    let request =
        match ipc::receive_request(stream, Instant::now() + ipc::TRANSFER_TIMEOUT, shutdown) {
            Ok(request) => request,
            Err(error) => {
                tracing::debug!("discarding incomplete IPC connection: {error}");
                return;
            }
        };
    let deadline = Instant::now() + ipc::COMMAND_TIMEOUT;
    let control = Arc::new(RequestControl::queued());
    let (reply, response) = sync_channel(1);
    let request = Request {
        argv: request.argv,
        cwd: request.cwd,
        reply,
        deadline,
        control: Arc::clone(&control),
    };
    if let Err(error) = requests.try_send(request) {
        control.cancel();
        let response = match error {
            TrySendError::Full(_) => protocol_error("the running instance is busy"),
            TrySendError::Disconnected(_) => {
                protocol_error("the running instance is shutting down")
            }
        };
        answer(stream, &response, shutdown, &control);
        return;
    }

    let response = wait_for_response(&response, shutdown, deadline, &control);
    answer(stream, &response, shutdown, &control);
}

fn wait_for_response(
    response: &Receiver<Response>,
    shutdown: &AtomicBool,
    deadline: Instant,
    control: &RequestControl,
) -> Response {
    loop {
        if shutdown.load(Ordering::Acquire) {
            control.cancel();
            break protocol_error("the running instance is shutting down");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            control.cancel();
            break protocol_error("the forwarded command timed out");
        }
        match response.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(response) => break response,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                control.cancel();
                break protocol_error("the forwarded command did not produce a response");
            }
        }
    }
}

fn answer(
    stream: &mut (impl std::io::Read + std::io::Write),
    response: &Response,
    shutdown: &Arc<AtomicBool>,
    control: &RequestControl,
) {
    if shutdown.load(Ordering::Acquire) {
        control.cancel();
        return;
    }
    if let Err(error) = ipc::send_response(
        stream,
        response,
        Instant::now() + ipc::TRANSFER_TIMEOUT,
        shutdown,
    ) {
        control.cancel();
        tracing::debug!("could not answer a forwarded command: {error}");
        return;
    }
    if let Err(error) = ipc::receive_ack(stream, Instant::now() + ipc::TRANSFER_TIMEOUT, shutdown) {
        control.cancel();
        tracing::debug!("forwarded command response was not acknowledged: {error}");
    }
}

fn protocol_error(message: &str) -> Response {
    let error = CliError::ipc(message);
    response_from_error(
        error.exit().code(),
        Reporter::new(false, false).render_error("ipc", &error),
    )
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn verify_private_directory(path: &Path) -> CliResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        CliError::ipc(format!(
            "could not inspect the IPC directory {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CliError::ipc(format!(
            "the IPC directory {} is not a real directory",
            path.display()
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(CliError::ipc(format!(
            "the IPC directory {} is accessible to other users; use a private 0700 directory",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn clear_stale(path: &Path) -> CliResult<()> {
    use std::{
        io::ErrorKind,
        os::unix::{fs::FileTypeExt as _, net::UnixStream},
    };

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(CliError::ipc(format!(
                "could not inspect existing IPC endpoint {}: {error}",
                path.display()
            )));
        }
    };
    if !metadata.file_type().is_socket() {
        return Err(CliError::ipc(format!(
            "refusing to replace non-socket IPC endpoint {}",
            path.display()
        )));
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(CliError::ipc(format!(
            "another Scrozz is already running and listening at {}",
            path.display()
        ))),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound
            ) =>
        {
            std::fs::remove_file(path).map_err(|remove_error| {
                CliError::ipc(format!(
                    "could not remove stale IPC endpoint {}: {remove_error}",
                    path.display()
                ))
            })
        }
        Err(error) => Err(CliError::ipc(format!(
            "could not verify existing IPC endpoint {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    #[test]
    fn forwarded_output_uses_the_same_renderer_as_local_output() {
        let (_, response) = run(&argv(&["capture", "--dry-run"]), None);
        assert_eq!(response.code, 0);
        assert!(response.stderr.is_empty());
        assert!(response.stdout.ends_with(b"\n"));
        assert!(String::from_utf8_lossy(&response.stdout).contains("Would capture"));

        let (_, response) = run(&argv(&["--json", "list", "displays"]), None);
        assert_ne!(response.code, 0);
        assert!(response.stderr.is_empty());
        assert!(response.stdout.ends_with(b"\n"));
        assert!(String::from_utf8_lossy(&response.stdout).contains("\"ok\":false"));
    }

    #[test]
    fn invalid_and_empty_commands_are_answered_on_stderr() {
        for args in [argv(&[]), argv(&["nonsuch"])] {
            let (command, response) = run(&args, None);
            assert!(command.is_none());
            assert_eq!(response.code, 2);
            assert!(response.stdout.is_empty());
            assert!(!response.stderr.is_empty());
        }
    }

    #[test]
    fn quiet_success_and_human_cancellation_are_silent() {
        let (_, response) = run(&argv(&["--quiet", "capture", "--dry-run"]), None);
        assert_eq!(response.code, 0);
        assert!(response.stdout.is_empty());
        assert!(response.stderr.is_empty());
    }

    #[test]
    fn working_directory_is_restored() {
        let before = std::env::current_dir().unwrap();
        let temporary = std::env::temp_dir();
        drop(WorkingDirectory::enter(Some(&temporary)).unwrap());
        assert_eq!(std::env::current_dir().unwrap(), before);
    }

    fn dry_run_request(
        deadline: Instant,
        control: Arc<RequestControl>,
    ) -> (Request, Receiver<Response>) {
        let (reply, response) = sync_channel(1);
        (
            Request {
                argv: argv(&["capture", "--dry-run"]),
                cwd: None,
                reply,
                deadline,
                control,
            },
            response,
        )
    }

    #[test]
    fn cancelled_and_expired_requests_never_dispatch() {
        let cancelled = Arc::new(RequestControl::queued());
        cancelled.cancel();
        let (request, response) = dry_run_request(
            Instant::now() + ipc::COMMAND_TIMEOUT,
            Arc::clone(&cancelled),
        );
        assert!(request.serve().is_none());
        assert!(cancelled.is_cancelled());
        assert!(matches!(
            response.try_recv(),
            Err(TryRecvError::Disconnected)
        ));

        let cancelled = Arc::new(RequestControl::queued());
        let (request, response) = dry_run_request(
            Instant::now() - Duration::from_millis(1),
            Arc::clone(&cancelled),
        );
        assert!(request.serve().is_none());
        assert!(cancelled.is_cancelled());
        assert!(matches!(
            response.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn cancellation_and_dispatch_claim_are_atomically_ordered() {
        let cancelled_first = RequestControl::queued();
        cancelled_first.cancel();
        assert!(!cancelled_first.try_start(Instant::now() + ipc::COMMAND_TIMEOUT));
        assert_eq!(
            cancelled_first.phase.load(Ordering::Acquire),
            REQUEST_CANCELLED
        );

        let dispatch_first = RequestControl::queued();
        assert!(dispatch_first.try_start(Instant::now() + ipc::COMMAND_TIMEOUT));
        dispatch_first.cancel();
        assert_eq!(
            dispatch_first.phase.load(Ordering::Acquire),
            REQUEST_RUNNING
        );
        assert!(dispatch_first.is_cancelled());
    }

    #[test]
    fn timeout_shutdown_and_reply_loss_cancel_the_queued_request() {
        let shutdown = AtomicBool::new(false);
        let cancelled = RequestControl::queued();
        let (_reply, response) = sync_channel(1);
        let timeout = wait_for_response(
            &response,
            &shutdown,
            Instant::now() - Duration::from_millis(1),
            &cancelled,
        );
        assert!(cancelled.is_cancelled());
        assert!(String::from_utf8_lossy(&timeout.stderr).contains("timed out"));

        let shutdown = AtomicBool::new(true);
        let cancelled = RequestControl::queued();
        let (_reply, response) = sync_channel(1);
        let stopped = wait_for_response(
            &response,
            &shutdown,
            Instant::now() + ipc::COMMAND_TIMEOUT,
            &cancelled,
        );
        assert!(cancelled.is_cancelled());
        assert!(String::from_utf8_lossy(&stopped.stderr).contains("shutting down"));

        let shutdown = AtomicBool::new(false);
        let cancelled = RequestControl::queued();
        let (reply, response) = sync_channel(1);
        drop(reply);
        let disconnected = wait_for_response(
            &response,
            &shutdown,
            Instant::now() + ipc::COMMAND_TIMEOUT,
            &cancelled,
        );
        assert!(cancelled.is_cancelled());
        assert!(
            String::from_utf8_lossy(&disconnected.stderr).contains("did not produce a response")
        );
    }

    #[cfg(unix)]
    fn endpoint_for(name: &str) -> (PathBuf, PathBuf) {
        let directory = std::env::temp_dir().join(format!("scrozz-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("instance.sock");
        (directory, path)
    }

    #[cfg(windows)]
    fn endpoint_for(name: &str) -> (PathBuf, PathBuf) {
        let path = PathBuf::from(format!(
            r"\\.\pipe\scrozz-test-{name}-{}",
            std::process::id()
        ));
        (PathBuf::new(), path)
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn duplicate_live_server_is_refused() {
        let (directory, path) = endpoint_for("duplicate");
        let first = Server::bind_at(path.clone()).expect("first server");
        assert!(Server::bind_at(path).is_err());
        drop(first);
        #[cfg(unix)]
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn unix_server_removes_its_endpoint() {
        let (directory, path) = endpoint_for("drop");
        {
            let server = Server::bind_at(path.clone()).unwrap();
            assert!(server.path().exists());
        }
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn unix_server_refuses_to_delete_a_non_socket_endpoint() {
        let (directory, path) = endpoint_for("regular-file");
        std::fs::write(&path, b"keep me").unwrap();
        assert!(Server::bind_at(path.clone()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep me");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn unix_server_refuses_a_shared_parent_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = std::env::temp_dir().join(format!("scrozz-shared-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Server::bind_at(directory.join("instance.sock")).is_err());
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o755,
            "binding must not chmod a caller-owned directory"
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn forwarded_command_round_trips_over_the_native_transport() {
        let _env = crate::test_env::lock();
        let (directory, path) = endpoint_for("roundtrip");
        let server = Server::bind_at(path.clone()).unwrap();
        crate::test_env::set(ipc::ENDPOINT_ENV, &path.to_string_lossy());
        let client = std::thread::spawn(|| ipc::forward(&argv(&["capture", "--dry-run"])));

        let request = loop {
            if let Some(request) = server.poll() {
                break request;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        assert!(matches!(request.serve(), Some(Command::Capture(_))));
        let response = client.join().unwrap().unwrap();
        assert_eq!(response.code, 0);
        assert!(response.stderr.is_empty());
        assert!(response.stdout.ends_with(b"\n"));

        drop(server);
        #[cfg(unix)]
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(windows)]
    #[test]
    fn a_probe_connection_does_not_poison_the_named_pipe() {
        let _env = crate::test_env::lock();
        let (_directory, path) = endpoint_for("probe");
        let server = Server::bind_at(path.clone()).unwrap();
        crate::test_env::set(ipc::ENDPOINT_ENV, &path.to_string_lossy());
        assert_eq!(ipc::probe(), ipc::Status::Running);

        let client = std::thread::spawn(|| ipc::forward(&argv(&["capture", "--dry-run"])));
        let request = loop {
            if let Some(request) = server.poll() {
                break request;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        request.serve();
        assert_eq!(client.join().unwrap().unwrap().code, 0);
    }
}
