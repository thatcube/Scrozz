//! The single-instance listener.
//!
//! # The problem this solves
//!
//! Once the menu-bar app is running it owns things a second process cannot see:
//! the capture stack on screen, the recording in progress, the hotkey
//! registrations. A `scrozz capture` typed into a terminal at that moment must
//! therefore happen *inside* the running app while preserving command-line
//! semantics: explicit sinks and JSON automation bypass ambient GUI After
//! Capture actions and never open an overlay or editor unexpectedly. Its pixels
//! still join the capture stack the user is already looking at, so a forwarded
//! capture receives history identity, a bounded texture, and Pin to Screen.
//!
//! [`crate::ipc`] already has the client half — [`crate::ipc::forward`] is what
//! the terminal process calls, and `main` already routes through it. This module
//! is the half that answers, and it lives here rather than in `ipc.rs` because
//! it only exists while the GUI does.
//!
//! # Fidelity
//!
//! A forwarded command must produce byte-identical output to a local one; the
//! whole point is that a script cannot tell the difference. So the answer is
//! built exactly the way [`crate::report::Reporter::emit`] builds it — raw bytes
//! when there are raw bytes, the JSON envelope when `--json` was passed, the
//! trimmed human line otherwise — and the exit code is adopted verbatim.
//!
//! # Backpressure and ownership
//!
//! A transport worker owns each connection: it reads one length-prefixed
//! request frame, hands the parsed request to the app over a queue exactly one
//! deep, waits for the app's [`Response`], writes it back as one frame, and
//! waits for the client's acknowledgement. A request that arrives while one is
//! already queued receives an explicit busy error rather than growing the
//! queue. The full-resolution pixels a command produced are **moved** into the
//! app's capture pipeline before the success reply is written, so a caller is
//! never told a capture succeeded that the app then refused.
//!
//! # Non-blocking
//!
//! [`Server::poll`] never waits. It is called from the main thread's tick,
//! between servicing the tray and the hotkey queue, and a blocking `accept()`
//! there would freeze the menu bar until someone happened to run a command.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{
            Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError, channel,
            sync_channel,
        },
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use scrozz_core::Capture;

use crate::{
    cli::{Cli, Command},
    commands,
    fault::{CliError, CliResult},
    gui::action::CaptureKind,
    gui::card::SurfaceWaker,
    gui::selection::CaptureSelector,
    ipc::{self, DIRECT_AFTER_CAPTURE_POLICY, Response, StreamKind},
    report::{Report, error_envelope, success_envelope},
};

const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(1);
const REQUEST_READ_POLL: Duration = Duration::from_millis(100);
/// How many accepted requests may wait for the app at once.
///
/// One. The app services the queue every frame, so a deeper queue would only
/// let a burst of terminal invocations retain more full-resolution frames
/// before anything could refuse them.
const REQUEST_QUEUE_DEPTH: usize = 1;
const WORKER_POLL: Duration = Duration::from_millis(8);

/// The one full-resolution capture a forwarded command may produce.
///
/// Owned, because the point of the handoff is that the app takes the pixels
/// rather than keeping a second copy alive while the reply is written.
#[derive(Debug)]
pub struct ForwardedCapture {
    /// The card kind the target implies, which decides the pin's chrome.
    pub kind: CaptureKind,
    /// The moved full-resolution capture.
    pub capture: Capture,
}

/// A request from another process, waiting for its answer.
pub struct Request {
    /// The argument vector as typed, `argv[0]` included.
    pub argv: Vec<String>,
    /// The caller's working directory, so relative `--output` paths resolve
    /// against *their* directory rather than the daemon's.
    pub cwd: Option<PathBuf>,
    /// Explicitly says direct command semantics bypass ambient GUI actions.
    pub after_capture_policy: String,
    reply: SyncSender<Response>,
    delivery: Option<Receiver<()>>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("argv", &self.argv)
            .field("cwd", &self.cwd)
            .field("after_capture_policy", &self.after_capture_policy)
            .finish_non_exhaustive()
    }
}

impl Request {
    /// Runs a command, then completes required in-process work before replying.
    ///
    /// The hook exists for operations such as terminal unpinning whose durable
    /// worker write must be ordered after older queued writes, and for admitting
    /// a forwarded capture's pixels. A hook failure replaces the otherwise
    /// successful command response.
    ///
    /// `after_success` receives ownership of the command's optional capture.
    /// There can be at most one, which bounds retention per request and lets the
    /// app move the frame directly into its capture pipeline before success is
    /// visible to the client.
    pub fn serve_with(
        self,
        after_success: impl FnOnce(&Command, Option<ForwardedCapture>) -> CliResult<()>,
    ) -> Option<Command> {
        let (command, response, captured) = run(&self.argv, self.cwd.as_deref());
        self.finish(command, response, captured, after_success)
    }

    fn finish(
        self,
        command: Option<Command>,
        mut response: Response,
        captured: Option<ForwardedCapture>,
        after_success: impl FnOnce(&Command, Option<ForwardedCapture>) -> CliResult<()>,
    ) -> Option<Command> {
        if response.code == 0
            && let Some(command) = command.as_ref()
            && let Err(error) = after_success(command, captured)
        {
            response = Self::command_error_response(&self.argv, command, &error);
        }
        let succeeded = response.code == 0;
        self.reply(&response, false);
        command.filter(|_| succeeded)
    }

    fn command_error_response(argv: &[String], command: &Command, error: &CliError) -> Response {
        use clap::Parser as _;

        let mut with_argv0 = Vec::with_capacity(argv.len() + 1);
        with_argv0.push("scrozz".to_owned());
        with_argv0.extend_from_slice(argv);
        let json_requested = Cli::try_parse_from(with_argv0).is_ok_and(|cli| cli.global.json);
        if json_requested {
            let slug = command.slug();
            json(
                error.exit().code(),
                error_envelope(&slug, error).to_compact_string(),
            )
        } else {
            text(error.exit().code(), error.to_string())
        }
    }

    /// Runs an interactive command on a worker thread with a live selector.
    ///
    /// The selector contract is synchronous, so this cannot run on the main
    /// thread: the overlay it opens is painted by the very loop that would be
    /// blocked. Admission still happens on the main thread — `admissions`
    /// carries the command and its moved pixels there and waits for the answer,
    /// so the reply is written only once the app has taken the frame.
    fn serve_with_selector(
        self,
        selector: &dyn CaptureSelector,
        admissions: &Sender<Admission>,
        shutters: &Sender<Sender<()>>,
        waker: Option<&SurfaceWaker>,
        stop: &AtomicBool,
    ) -> Option<Command> {
        let mut announce_acquisition = || {
            let (acknowledged, acknowledgement) = channel();
            shutters.send(acknowledged).map_err(|_| {
                CliError::ipc("the running instance stopped before shutter feedback")
            })?;
            if let Some(waker) = waker {
                waker();
            }
            acknowledgement
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| {
                    CliError::ipc("the running instance did not acknowledge shutter feedback")
                })
        };
        let (command, response, captured) = run_with_selector(
            &self.argv,
            self.cwd.as_deref(),
            Some(selector),
            &mut announce_acquisition,
        );
        self.finish(command, response, captured, |command, captured| {
            admit_on_main_thread(admissions, waker, stop, command, captured)
        })
    }

    /// Answers with a result produced somewhere other than this request.
    ///
    /// `record --stop` cannot be answered by *running* `record --stop`: the
    /// answer is whatever the recording that is now finalising eventually
    /// produced, which arrives on another thread some time later. So the
    /// request is parked, and this is how it is finally answered — with the
    /// same envelope, stream choice and exit code a local run would have used,
    /// because the caller must not be able to tell the difference.
    pub fn answer(self, result: &CliResult<Report>) {
        self.answer_with_delivery(result, false);
    }

    /// Answers and waits until the transport has written the reply and received
    /// its ACK. Used only when the caller is about to drop the owning server.
    pub fn answer_and_wait_delivery(self, result: &CliResult<Report>) {
        self.answer_with_delivery(result, true);
    }

    fn answer_with_delivery(self, result: &CliResult<Report>, wait_for_delivery: bool) {
        use clap::Parser as _;

        let mut with_argv0 = Vec::with_capacity(self.argv.len() + 1);
        with_argv0.push("scrozz".to_owned());
        with_argv0.extend_from_slice(&self.argv);
        let parsed = Cli::try_parse_from(with_argv0).ok();
        let slug = parsed
            .as_ref()
            .and_then(|cli| cli.command.as_ref())
            .map_or_else(|| Command::Gui.slug(), Command::slug);
        let json_requested = parsed.as_ref().is_some_and(|cli| cli.global.json);
        let quiet = parsed.as_ref().is_some_and(|cli| cli.global.quiet);

        let response = match result {
            Ok(report) => {
                if let Some(bytes) = report.raw.clone() {
                    Response {
                        code: 0,
                        stream: StreamKind::Binary,
                        payload: bytes,
                    }
                } else if json_requested {
                    json(
                        0,
                        success_envelope(&slug, report.data.clone()).to_compact_string(),
                    )
                } else if quiet {
                    text(0, String::new())
                } else {
                    text(0, report.human.trim_end().to_owned())
                }
            }
            Err(error) => {
                let code = error.exit().code();
                if json_requested {
                    json(code, error_envelope(&slug, error).to_compact_string())
                } else {
                    text(code, error.to_string())
                }
            }
        };
        self.reply(&response, wait_for_delivery);
    }

    fn reply(self, response: &Response, wait_for_delivery: bool) {
        if self.reply.send(response.clone()).is_err() {
            tracing::debug!("forwarded command client disconnected before the reply");
            return;
        }
        if wait_for_delivery
            && let Some(delivery) = self.delivery
            && delivery.recv_timeout(ipc::COMMAND_TIMEOUT).is_err()
        {
            tracing::debug!("forwarded command transport ended before reply delivery completed");
        }
    }
}

/// Executes a forwarded argument vector, exactly as a local run would.
///
/// Separated from [`Request`] so the fidelity rules can be tested without a
/// socket, which is the part most likely to drift from
/// [`crate::report::Reporter::emit`].
fn run(
    argv: &[String],
    cwd: Option<&Path>,
) -> (Option<Command>, Response, Option<ForwardedCapture>) {
    run_with_selector(argv, cwd, None, &mut || Ok(()))
}

fn run_with_selector(
    argv: &[String],
    cwd: Option<&Path>,
    selector: Option<&dyn CaptureSelector>,
    acquired: &mut dyn FnMut() -> CliResult<()>,
) -> (Option<Command>, Response, Option<ForwardedCapture>) {
    use clap::Parser as _;

    if argv.is_empty() {
        // A forwarded invocation with nothing in it cannot be honoured, and
        // clap would answer with the help text rather than saying so.
        return (
            None,
            text(2, "the forwarded command had no arguments".to_owned()),
            None,
        );
    }

    // The wire carries the arguments *after* the program name — `ipc::forward`
    // is handed `env::args().skip(1)`. clap always treats its first element as
    // the program name and discards it, so without a placeholder the real
    // subcommand is eaten and every forwarded command parses as a bare
    // `scrozz`. That failure is invisible: the response comes back well-formed,
    // for the wrong command.
    let mut with_argv0 = Vec::with_capacity(argv.len() + 1);
    with_argv0.push("scrozz".to_owned());
    with_argv0.extend_from_slice(argv);

    let mut cli = match Cli::try_parse_from(&with_argv0) {
        Ok(cli) => cli,
        // clap's own rejection. There is no slug to report it under, because we
        // never got as far as knowing which subcommand was meant.
        Err(err) => return (None, text(2, err.to_string()), None),
    };
    // Relative paths belong to the caller's directory, not the daemon's.
    // Rewriting the arguments rather than changing the process working
    // directory keeps the GUI's own directory untouched, which matters because
    // every later capture would otherwise inherit whatever directory the last
    // forwarded command happened to run in.
    let aliases = cwd.map_or_else(Default::default, |cwd| cli.absolutize_paths(cwd));

    let command = cli.command.clone().unwrap_or(Command::Gui);
    let slug = command.slug();

    let mut captured = None;
    let result = cli.validate().and_then(|()| match selector {
        Some(selector) => commands::dispatch_observed_with_selector_and_acquisition(
            &command,
            selector,
            &mut |kind, capture| retain_capture(&mut captured, kind, capture),
            acquired,
        ),
        None => commands::dispatch_observed(&command, &mut |kind, capture| {
            retain_capture(&mut captured, kind, capture)
        }),
    });

    let response = match result {
        Ok(mut report) => {
            aliases.restore_json(&mut report.data);
            report.human = aliases.restore_text(&report.human);
            if let Some(bytes) = report.raw {
                Response {
                    code: 0,
                    stream: StreamKind::Binary,
                    payload: bytes,
                }
            } else if cli.global.json {
                json(0, success_envelope(&slug, report.data).to_compact_string())
            } else if cli.global.quiet {
                text(0, String::new())
            } else {
                text(0, report.human.trim_end().to_owned())
            }
        }
        Err(err) => {
            let code = err.exit().code();
            if cli.global.json {
                let mut document = error_envelope(&slug, &err);
                aliases.restore_json(&mut document);
                json(code, document.to_compact_string())
            } else {
                text(code, aliases.restore_text(&err.to_string()))
            }
        }
    };

    // A command that failed produced nothing the app should show. Dropping the
    // frame here also releases it before the reply, rather than holding a
    // full-resolution allocation alive for a request that is already lost.
    if response.code != 0 {
        captured = None;
    }
    (Some(command), response, captured)
}

/// Records the one capture a request is allowed to hand over.
///
/// A second one is a bug in the command layer rather than a user error, but it
/// is reported rather than ignored: silently dropping pixels would make a
/// capture vanish, and silently keeping both would defeat the retention bound.
fn retain_capture(
    slot: &mut Option<ForwardedCapture>,
    kind: CaptureKind,
    capture: Capture,
) -> CliResult<()> {
    if slot.is_some() {
        return Err(CliError::ipc(
            "a forwarded command produced more than one full-resolution capture",
        ));
    }
    *slot = Some(ForwardedCapture { kind, capture });
    Ok(())
}

/// Work the main thread must complete before a forwarded command is answered.
pub struct Admission {
    command: Command,
    captured: Option<ForwardedCapture>,
    outcome: SyncSender<CliResult<()>>,
}

impl Admission {
    /// The command that ran, for the app's own bookkeeping.
    #[must_use]
    pub const fn command(&self) -> &Command {
        &self.command
    }

    /// Takes the moved pixels, if the command produced any.
    pub fn take_capture(&mut self) -> Option<ForwardedCapture> {
        self.captured.take()
    }

    /// Whether this admission still owns full-resolution pixels.
    #[must_use]
    pub const fn has_capture(&self) -> bool {
        self.captured.is_some()
    }

    /// Reports the decision, and yields the command when it stands.
    ///
    /// The worker is blocked on this answer, so it must be sent exactly once.
    pub fn complete(self, result: CliResult<()>) -> Option<Command> {
        let accepted = result.is_ok();
        if self.outcome.send(result).is_err() {
            tracing::debug!("the forwarded-command worker stopped before admission completed");
        }
        accepted.then_some(self.command)
    }
}

fn admit_on_main_thread(
    admissions: &Sender<Admission>,
    waker: Option<&SurfaceWaker>,
    stop: &AtomicBool,
    command: &Command,
    captured: Option<ForwardedCapture>,
) -> CliResult<()> {
    let (outcome, answer) = sync_channel(1);
    let admission = Admission {
        command: command.clone(),
        captured,
        outcome,
    };
    if admissions.send(admission).is_err() {
        return Err(CliError::ipc(
            "the running instance stopped before it could accept the command",
        ));
    }
    if let Some(waker) = waker {
        waker();
    }

    let deadline = Instant::now() + ipc::COMMAND_TIMEOUT;
    loop {
        if stop.load(Ordering::Acquire) {
            return Err(CliError::ipc(
                "the running instance is shutting down and did not accept the command",
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CliError::ipc(
                "the running instance did not accept the command in time",
            ));
        }
        match answer.recv_timeout(remaining.min(REQUEST_READ_POLL)) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(CliError::ipc(
                    "the running instance dropped the command before accepting it",
                ));
            }
        }
    }
}

enum ForwardJob {
    Serve(Request),
    Stop,
}

/// Serial command executor for requests accepted on the UI thread.
///
/// Interactive selection is synchronous by contract. Running it here lets the
/// worker wait on the selector while eframe's main thread continues polling the
/// selector bridge and painting the overlay. Anything the command produced is
/// still admitted by the main thread, through [`Forwarder::poll`], before the
/// worker answers the client.
pub struct Forwarder {
    jobs: Sender<ForwardJob>,
    admissions: Receiver<Admission>,
    shutters: Receiver<Sender<()>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Forwarder {
    /// Starts the forwarded-command worker.
    ///
    /// The waker is what makes the handoff work on an event-driven display: the
    /// worker blocks until the main thread has accepted the command, and
    /// without a wake an idle loop would not run the tick that accepts it.
    ///
    /// # Errors
    ///
    /// Returns a platform error if the thread cannot be created.
    pub fn start(
        selector: Arc<dyn CaptureSelector>,
        waker: Option<SurfaceWaker>,
    ) -> CliResult<Self> {
        let (jobs, requests) = channel();
        let (admitted, admissions) = channel();
        let (shuttered, shutters) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("scrozz-forwarded-command".to_owned())
            .spawn(move || {
                while let Ok(job) = requests.recv() {
                    match job {
                        ForwardJob::Serve(request) => {
                            request.serve_with_selector(
                                selector.as_ref(),
                                &admitted,
                                &shuttered,
                                waker.as_ref(),
                                &worker_stop,
                            );
                        }
                        ForwardJob::Stop => break,
                    }
                }
            })
            .map_err(|error| {
                CliError::Core(scrozz_core::Error::Platform(format!(
                    "could not start the forwarded-command worker: {error}"
                )))
            })?;
        Ok(Self {
            jobs,
            admissions,
            shutters,
            stop,
            worker: Some(worker),
        })
    }

    /// Queues an accepted request without blocking the caller.
    pub fn submit(&self, request: Request) -> bool {
        self.jobs.send(ForwardJob::Serve(request)).is_ok()
    }

    /// Takes one completed command awaiting the app's acceptance, if any.
    pub fn poll(&self) -> Option<Admission> {
        self.admissions.try_recv().ok()
    }

    /// Drains acquisition notices that must play on the GUI's main thread.
    pub fn drain_shutters(&self) -> Vec<Sender<()>> {
        self.shutters.try_iter().collect()
    }

    /// Stops after any currently executing command has returned.
    pub fn stop(&mut self) {
        // Released first, so a worker blocked on an admission this app will
        // never service stops waiting instead of holding the join forever.
        self.stop.store(true, Ordering::Release);
        let _ = self.jobs.send(ForwardJob::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn text(code: u8, body: String) -> Response {
    Response {
        code,
        stream: StreamKind::Text,
        payload: body.into_bytes(),
    }
}

fn json(code: u8, body: String) -> Response {
    Response {
        code,
        stream: StreamKind::Json,
        payload: body.into_bytes(),
    }
}

/// The listener a running GUI holds.
pub struct Server {
    path: PathBuf,
    requests: Receiver<Request>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

#[cfg(unix)]
fn ensure_private_socket_directory(path: &Path) -> CliResult<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(CliError::ipc(format!(
                    "instance socket parent {} is not a real directory",
                    path.display()
                )));
            }
            // SAFETY: `geteuid` is a side-effect-free POSIX process query.
            if metadata.uid() != unsafe { geteuid() } {
                return Err(CliError::ipc(format!(
                    "instance socket parent {} is owned by another user",
                    path.display()
                )));
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
                    |error| {
                        CliError::ipc(format!(
                            "could not restrict instance socket directory {}: {error}",
                            path.display()
                        ))
                    },
                )?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(path).map_err(|error| {
                CliError::ipc(format!(
                    "could not make private instance socket directory {}: {error}",
                    path.display()
                ))
            })?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    CliError::ipc(format!(
                        "could not restrict instance socket directory {}: {error}",
                        path.display()
                    ))
                },
            )?;
        }
        Err(error) => {
            return Err(CliError::ipc(format!(
                "could not inspect instance socket directory {}: {error}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
unsafe extern "C" {
    fn geteuid() -> u32;
}

#[cfg(target_os = "macos")]
fn peer_matches_owner(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd as _;

    unsafe extern "C" {
        fn getpeereid(socket: i32, effective_user: *mut u32, effective_group: *mut u32) -> i32;
    }
    let mut user = 0_u32;
    let mut group = 0_u32;
    // SAFETY: both output pointers are valid and the stream owns a live socket.
    unsafe { getpeereid(stream.as_raw_fd(), &mut user, &mut group) == 0 && user == geteuid() }
}

#[cfg(target_os = "linux")]
fn peer_matches_owner(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::ffi::c_void;
    use std::os::fd::AsRawFd as _;

    #[repr(C)]
    struct UCred {
        pid: i32,
        uid: u32,
        gid: u32,
    }
    unsafe extern "C" {
        fn getsockopt(
            socket: i32,
            level: i32,
            option: i32,
            value: *mut c_void,
            length: *mut u32,
        ) -> i32;
    }
    let mut credentials = UCred {
        pid: 0,
        uid: u32::MAX,
        gid: 0,
    };
    let mut length = u32::try_from(std::mem::size_of::<UCred>()).unwrap_or(u32::MAX);
    // Linux uapi constants: SOL_SOCKET=1, SO_PEERCRED=17.
    let status = unsafe {
        getsockopt(
            stream.as_raw_fd(),
            1,
            17,
            std::ptr::from_mut(&mut credentials).cast(),
            &mut length,
        )
    };
    status == 0
        && length as usize == std::mem::size_of::<UCred>()
        // SAFETY: `geteuid` is a side-effect-free POSIX process query.
        && credentials.uid == unsafe { geteuid() }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn peer_matches_owner(_stream: &std::os::unix::net::UnixStream) -> bool {
    true
}

impl Server {
    /// Binds the endpoint, taking over a stale socket if one is left behind.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Ipc`] if another instance already holds the endpoint
    /// — not a fault but a fact the caller must act on, by forwarding rather
    /// than starting a second menu-bar app.
    pub fn bind() -> CliResult<Self> {
        Self::bind_with_waker(None)
    }

    /// Binds the default endpoint and wakes the window only when a request arrives.
    ///
    /// # Errors
    ///
    /// As [`Server::bind`].
    pub fn bind_with_waker(waker: Option<SurfaceWaker>) -> CliResult<Self> {
        Self::bind_at_with_waker(ipc::endpoint(), waker)
    }

    /// Binds a specific path.
    ///
    /// Exposed because tests must not fight over the real endpoint with a GUI
    /// the developer happens to be running.
    ///
    /// # Errors
    ///
    /// As [`Server::bind`].
    #[cfg(unix)]
    pub fn bind_at(path: PathBuf) -> CliResult<Self> {
        Self::bind_at_with_waker(path, None)
    }

    #[cfg(unix)]
    fn bind_at_with_waker(path: PathBuf, waker: Option<SurfaceWaker>) -> CliResult<Self> {
        use std::os::unix::fs::PermissionsExt as _;
        use std::os::unix::net::UnixListener;

        if let Some(parent) = path.parent() {
            ensure_private_socket_directory(parent)?;
        }

        clear_stale(&path)?;

        let listener = UnixListener::bind(&path)
            .map_err(|e| CliError::ipc(format!("could not listen at {}: {e}", path.display())))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            CliError::ipc(format!(
                "could not restrict instance socket {} to its owner: {e}",
                path.display()
            ))
        })?;
        listener.set_nonblocking(true).map_err(|e| {
            CliError::ipc(format!("could not make the instance socket pollable: {e}"))
        })?;

        spawn(path, waker, move |requests, stop, waker| {
            unix_worker(&listener, &requests, &stop, waker.as_ref());
        })
    }

    /// Binds the current user's protected named pipe.
    ///
    /// # Errors
    ///
    /// As [`Server::bind`].
    #[cfg(windows)]
    pub fn bind_at(path: PathBuf) -> CliResult<Self> {
        Self::bind_at_with_waker(path, None)
    }

    #[cfg(windows)]
    fn bind_at_with_waker(path: PathBuf, waker: Option<SurfaceWaker>) -> CliResult<Self> {
        let listener = ipc::windows_pipe::PipeListener::bind(&path)?;
        spawn(path, waker, move |requests, stop, waker| {
            windows_worker(listener, &requests, &stop, waker.as_ref());
        })
    }

    /// There is nothing to listen on, so the GUI runs without forwarding.
    ///
    /// # Errors
    ///
    /// Never. The GUI runs without single-instance forwarding rather than
    /// refusing to start over it.
    #[cfg(not(any(unix, windows)))]
    pub fn bind_at(path: PathBuf) -> CliResult<Self> {
        Self::bind_at_with_waker(path, None)
    }

    #[cfg(not(any(unix, windows)))]
    fn bind_at_with_waker(path: PathBuf, _waker: Option<SurfaceWaker>) -> CliResult<Self> {
        tracing::warn!(
            "this build has no single-instance listener, so `scrozz capture` from \
             a terminal will run in its own process"
        );
        let (_sender, requests) = sync_channel(REQUEST_QUEUE_DEPTH);
        Ok(Self {
            path,
            requests,
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        })
    }

    /// Takes one pending request, if there is one. Never blocks.
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

    /// Where this server is listening.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        #[cfg(unix)]
        {
            // Otherwise the next launch finds a socket file with nothing behind
            // it and has to decide whether it is stale — solvable, but this is
            // free. A Windows pipe disappears with its last handle.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(any(unix, windows))]
fn spawn(
    path: PathBuf,
    waker: Option<SurfaceWaker>,
    run_worker: impl FnOnce(SyncSender<Request>, Arc<AtomicBool>, Option<SurfaceWaker>) + Send + 'static,
) -> CliResult<Server> {
    let (request_tx, requests) = sync_channel(REQUEST_QUEUE_DEPTH);
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = std::thread::Builder::new()
        .name("scrozz-ipc-listener".into())
        .spawn(move || run_worker(request_tx, worker_stop, waker))
        .map_err(|error| {
            CliError::ipc(format!(
                "could not start the instance listener for {}: {error}",
                path.display()
            ))
        })?;

    tracing::debug!(path = %path.display(), "listening for forwarded commands");
    Ok(Server {
        path,
        requests,
        stop,
        worker: Some(worker),
    })
}

#[cfg(unix)]
fn unix_worker(
    listener: &std::os::unix::net::UnixListener,
    requests: &SyncSender<Request>,
    stop: &Arc<AtomicBool>,
    waker: Option<&SurfaceWaker>,
) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Filesystem permissions already restrict the socket, but the
                // kernel's own view of the peer is the check that cannot be
                // raced by a directory that was writable a moment ago.
                if !peer_matches_owner(&stream) {
                    tracing::warn!("rejected an instance-socket client owned by another user");
                    continue;
                }
                if let Err(error) = stream.set_nonblocking(false) {
                    tracing::warn!("could not make an accepted command socket blocking: {error}");
                    continue;
                }
                if let Err(error) = stream.set_read_timeout(Some(REQUEST_READ_POLL)) {
                    tracing::warn!("could not bound a forwarded-command read: {error}");
                    continue;
                }
                if let Err(error) = stream.set_write_timeout(Some(REQUEST_READ_POLL)) {
                    tracing::warn!("could not bound a forwarded-command reply: {error}");
                    continue;
                }
                serve_connection(&mut stream, requests, stop, waker);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(WORKER_POLL);
            }
            Err(error) => {
                tracing::warn!("could not accept a forwarded command: {error}");
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
}

#[cfg(windows)]
fn windows_worker(
    mut listener: ipc::windows_pipe::PipeListener,
    requests: &SyncSender<Request>,
    stop: &Arc<AtomicBool>,
    waker: Option<&SurfaceWaker>,
) {
    while !stop.load(Ordering::Acquire) {
        if let Err(error) = listener.accept(stop) {
            if !stop.load(Ordering::Acquire) {
                tracing::warn!("{error}");
            }
            std::thread::sleep(WORKER_POLL);
            continue;
        }
        serve_connection(&mut listener, requests, stop, waker);
        listener.disconnect();
    }
}

#[cfg(any(unix, windows))]
fn serve_connection(
    stream: &mut (impl std::io::Read + std::io::Write),
    requests: &SyncSender<Request>,
    stop: &Arc<AtomicBool>,
    waker: Option<&SurfaceWaker>,
) {
    let Some((argv, cwd, after_capture_policy)) = read_request(stream, stop) else {
        return;
    };
    let (reply, response) = sync_channel(1);
    let (delivered, delivery) = sync_channel(1);
    let request = Request {
        argv,
        cwd,
        after_capture_policy,
        reply,
        delivery: Some(delivery),
    };
    if let Err(error) = requests.try_send(request) {
        let message = match error {
            TrySendError::Full(_) => "the running instance is busy",
            TrySendError::Disconnected(_) => "the running instance is shutting down",
        };
        answer(stream, &protocol_error(message), stop);
        return;
    }
    if let Some(waker) = waker {
        waker();
    }

    let deadline = Instant::now() + ipc::COMMAND_TIMEOUT;
    let response = loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break protocol_error("the forwarded command timed out");
        }
        match response.recv_timeout(remaining.min(REQUEST_READ_POLL)) {
            Ok(response) => break response,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                break protocol_error("the forwarded command did not produce a response");
            }
        }
    };
    answer(stream, &response, stop);
    let _ = delivered.send(());
}

#[cfg(any(unix, windows))]
fn protocol_error(message: &str) -> Response {
    let error = CliError::ipc(message);
    text(error.exit().code(), error.to_string())
}

#[cfg(any(unix, windows))]
fn answer(
    stream: &mut (impl std::io::Read + std::io::Write),
    response: &Response,
    stop: &AtomicBool,
) {
    if let Err(error) = ipc::send_response_frame(
        stream,
        response,
        Instant::now() + ipc::TRANSFER_TIMEOUT,
        stop,
    ) {
        tracing::debug!("could not answer a forwarded command: {error}");
        return;
    }
    if let Err(error) = ipc::receive_ack(stream, Instant::now() + ipc::TRANSFER_TIMEOUT, stop) {
        tracing::debug!("forwarded command response was not acknowledged: {error}");
    }
}

/// Reads and validates one framed request.
///
/// Everything here arrives from outside the process, so every field is bounded
/// and a malformed line is discarded rather than guessed at.
#[cfg(any(unix, windows, test))]
fn read_request(
    stream: &mut impl std::io::Read,
    stop: &AtomicBool,
) -> Option<(Vec<String>, Option<PathBuf>, String)> {
    let raw = match ipc::receive_request_frame(stream, Instant::now() + REQUEST_READ_TIMEOUT, stop)
    {
        Ok(raw) => raw,
        Err(error) => {
            tracing::debug!("discarding incomplete forwarded command: {error}");
            return None;
        }
    };
    let line = match std::str::from_utf8(&raw) {
        Ok(line) if line.ends_with('\n') => &line[..line.len() - 1],
        Ok(_) => {
            tracing::warn!("forwarded command was not newline terminated");
            return None;
        }
        Err(error) => {
            tracing::warn!("forwarded command was not valid UTF-8: {error}");
            return None;
        }
    };
    let schema = integer_field(line, "schema")?;
    if schema != ipc::REQUEST_SCHEMA {
        tracing::warn!(
            schema,
            expected = ipc::REQUEST_SCHEMA,
            "refusing a forwarded request with an unsupported schema"
        );
        return None;
    }
    let argv = string_array(line, "argv")?;
    if argv.len() > 4_096 || argv.iter().any(|argument| argument.contains('\0')) {
        tracing::warn!("forwarded command arguments were malformed or excessive");
        return None;
    }
    let cwd = string_field(line, "cwd").map(PathBuf::from);
    if cwd.as_ref().is_some_and(|path| {
        let raw = path.to_string_lossy();
        raw.len() > 32 * 1024 || raw.contains('\0')
    }) {
        tracing::warn!("forwarded command working directory was malformed or excessive");
        return None;
    }
    let after_capture_policy = string_field(line, "after_capture_policy")?;
    if after_capture_policy != DIRECT_AFTER_CAPTURE_POLICY {
        tracing::warn!(
            policy = %after_capture_policy,
            "refusing a forwarded request with an unknown After Capture policy"
        );
        return None;
    }
    Some((argv, cwd, after_capture_policy))
}

/// Removes a socket file left behind by a crash.
///
/// Done by connecting rather than by a lock file: a lock file records what a
/// process *intended*, and a killed process leaves one behind saying it is still
/// running. A connection refused is proof.
#[cfg(unix)]
fn clear_stale(path: &Path) -> CliResult<()> {
    use std::os::unix::net::UnixStream;

    if !path.exists() {
        return Ok(());
    }
    if UnixStream::connect(path).is_ok() {
        return Err(CliError::ipc(format!(
            "another Scrozz is already running and listening at {}",
            path.display()
        )));
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}
/// Pulls a JSON string array out of the request line.
///
/// A full parser would be nicer, but this crate has no JSON reader and the input
/// is not arbitrary: it is produced by [`crate::ipc::encode_request`] in the same
/// protocol version, and the header check in
/// [`crate::ipc::parse_response`] guards the version. What matters is that a
/// malformed line yields `None` rather than a panic, because it arrives from
/// outside this process.
fn string_array(line: &str, key: &str) -> Option<Vec<String>> {
    let bytes = line.as_bytes();
    let mut at = line.find(&format!("\"{key}\":["))? + key.len() + 4;
    let mut values = Vec::new();

    loop {
        match bytes.get(at)? {
            b']' => return Some(values),
            b'"' => {
                let (value, end) = read_string(line, at + 1)?;
                values.push(value);
                at = end + 1;
            }
            // Whitespace and the separating commas.
            _ => at += 1,
        }
    }
}

/// Pulls a nullable JSON string field out of the request line.
fn string_field(line: &str, key: &str) -> Option<String> {
    let at = line.find(&format!("\"{key}\":\""))? + key.len() + 4;
    read_string(line, at).map(|(value, _)| value)
}

fn integer_field(line: &str, key: &str) -> Option<i64> {
    let at = line.find(&format!("\"{key}\":"))? + key.len() + 3;
    let digits: String = line[at..]
        .chars()
        .take_while(|character| character.is_ascii_digit() || *character == '-')
        .collect();
    digits.parse().ok()
}

/// Reads one JSON string body starting at `from`, just after the opening quote.
///
/// Returns the decoded value and the byte index of its closing quote.
fn read_string(line: &str, from: usize) -> Option<(String, usize)> {
    let bytes = line.as_bytes();
    let mut out = String::new();
    let mut at = from;

    while at < bytes.len() {
        match bytes[at] {
            b'"' => return Some((out, at)),
            b'\\' => {
                at += 1;
                let escaped = *bytes.get(at)?;
                match escaped {
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'u' => {
                        // `Json::str` only escapes control characters this way,
                        // so four hex digits and never a surrogate pair.
                        let hex = line.get(at + 1..at + 5)?;
                        out.push(char::from_u32(u32::from_str_radix(hex, 16).ok()?)?);
                        at += 4;
                    }
                    other => out.push(char::from(other)),
                }
                at += 1;
            }
            _ => {
                // Multi-byte characters pass through whole; indexing on a char
                // boundary is what makes the slice below safe.
                let rest = line.get(at..)?;
                let ch = rest.chars().next()?;
                out.push(ch);
                at += ch.len_utf8();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;
    use scrozz_core::{
        CaptureTarget, ColorSpace, Frame, PhysicalSize, PixelFormat, Provenance, ScaleFactor,
        WindowId,
    };

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    fn large_capture(marker: u8) -> Capture {
        let edge = 1_024;
        Capture {
            frame: Frame {
                data: vec![marker; edge * edge * 4],
                size: PhysicalSize::new(edge as f64, edge as f64),
                stride: edge * 4,
                format: PixelFormat::Rgba8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::IDENTITY,
            },
            provenance: Provenance::Window,
            target: CaptureTarget::Window(WindowId(format!("large-{marker}"))),
        }
    }

    fn request_for(argv: Vec<String>) -> (Request, Receiver<Response>) {
        let (reply, response) = sync_channel(1);
        (
            Request {
                argv,
                cwd: None,
                after_capture_policy: DIRECT_AFTER_CAPTURE_POLICY.to_owned(),
                reply,
                delivery: None,
            },
            response,
        )
    }

    #[test]
    fn an_argv_array_round_trips_through_the_wire_format() {
        let sent = argv(&["scrozz", "capture", "--json"]);
        let line = ipc::encode_request(&sent, None);
        assert_eq!(string_array(&line, "argv"), Some(sent));
    }

    #[test]
    fn a_cwd_round_trips_and_is_absent_when_not_sent() {
        let sent = argv(&["scrozz"]);
        let with = ipc::encode_request(&sent, Some(Path::new("/Users/someone/work")));
        assert_eq!(
            string_field(&with, "cwd").as_deref(),
            Some("/Users/someone/work")
        );

        let without = ipc::encode_request(&sent, None);
        assert_eq!(string_field(&without, "cwd"), None);
    }

    #[test]
    fn an_argument_containing_a_quote_survives() {
        // The case a naive split on `","` gets wrong.
        let sent = argv(&["scrozz", "capture", "-o", r#"/a "quoted" name.png"#]);
        let line = ipc::encode_request(&sent, None);
        assert_eq!(string_array(&line, "argv"), Some(sent));
    }

    #[test]
    fn an_argument_containing_a_backslash_survives() {
        let sent = argv(&["scrozz", r"C:\shots\a.png"]);
        let line = ipc::encode_request(&sent, None);
        assert_eq!(string_array(&line, "argv"), Some(sent));
    }

    #[test]
    fn a_non_ascii_argument_survives() {
        let sent = argv(&["scrozz", "capture", "-o", "/captures/écran ✅.png"]);
        let line = ipc::encode_request(&sent, None);
        assert_eq!(string_array(&line, "argv"), Some(sent));
    }

    #[test]
    fn an_empty_argv_is_an_empty_vector_not_a_failure() {
        let line = ipc::encode_request(&[], None);
        assert_eq!(string_array(&line, "argv"), Some(Vec::new()));
    }

    #[test]
    fn a_truncated_line_is_rejected_rather_than_panicking() {
        // This arrives from outside the process, so it must never abort us.
        assert_eq!(string_array(r#"{"argv":["scrozz"#, "argv"), None);
        assert_eq!(string_array(r#"{"argv":["#, "argv"), None);
        assert_eq!(string_array("{}", "argv"), None);
        assert_eq!(string_field(r#"{"cwd":"#, "cwd"), None);
        assert_eq!(string_field(r#"{"cwd":"unterminated"#, "cwd"), None);
    }

    #[test]
    fn a_missing_key_is_none() {
        let line = ipc::encode_request(&argv(&["scrozz"]), None);
        assert_eq!(string_array(&line, "nope"), None);
        assert_eq!(string_field(&line, "nope"), None);
    }

    #[test]
    fn one_request_retains_at_most_one_owned_full_resolution_capture() {
        let first = large_capture(1);
        let first_pixels = first.frame.data.as_ptr();
        let mut slot = None;
        retain_capture(&mut slot, CaptureKind::Region, first).expect("first capture");
        let retained = slot.as_ref().expect("capture retained");
        assert_eq!(
            retained.capture.frame.data.as_ptr(),
            first_pixels,
            "capture pixels must move into the request slot rather than clone"
        );

        let error = retain_capture(&mut slot, CaptureKind::Window, large_capture(2))
            .expect_err("a second full-resolution frame in one request must be rejected");
        assert!(error.to_string().contains("more than one"), "{error}");
    }

    #[test]
    fn acceptance_completes_before_a_success_reply_and_runs_once() {
        let command = Cli::try_parse_from([
            "scrozz",
            "capture",
            "--region",
            "0,0,10,10",
            "--output",
            "capture.png",
        ])
        .expect("valid command")
        .command
        .expect("capture command");
        let (request, response) = request_for(argv(&["capture", "--region", "0,0,10,10"]));
        let accepted = std::cell::Cell::new(0_u8);
        let result = request.finish(
            Some(command),
            text(0, "captured".to_owned()),
            Some(ForwardedCapture {
                kind: CaptureKind::Region,
                capture: large_capture(3),
            }),
            |_, captured| {
                assert!(matches!(response.try_recv(), Err(TryRecvError::Empty)));
                let captured = captured.expect("owned capture reaches acceptance");
                assert_eq!(captured.kind, CaptureKind::Region);
                accepted.set(accepted.get() + 1);
                Ok(())
            },
        );

        assert!(matches!(result, Some(Command::Capture(_))));
        assert_eq!(accepted.get(), 1, "acceptance must be exactly once");
        assert_eq!(response.recv().expect("success reply").code, 0);
    }

    #[test]
    fn acceptance_failure_replaces_success_before_the_reply() {
        let command = Cli::try_parse_from(["scrozz", "capture", "--window", "Editor"])
            .expect("valid command")
            .command
            .expect("capture command");
        let (request, response) = request_for(argv(&["capture", "--window", "Editor"]));

        let result = request.finish(
            Some(command),
            text(0, "captured".to_owned()),
            Some(ForwardedCapture {
                kind: CaptureKind::Window,
                capture: large_capture(4),
            }),
            |_, captured| {
                assert!(captured.is_some());
                Err(CliError::ipc("capture admission was full"))
            },
        );

        assert!(
            result.is_none(),
            "failed acceptance is not a successful command"
        );
        let response = response.recv().expect("failure reply");
        assert_ne!(response.code, 0);
        assert!(String::from_utf8_lossy(&response.payload).contains("capture admission was full"));
    }

    #[test]
    fn an_empty_argument_vector_is_answered_not_ignored() {
        let (command, response, captured) = run(&[], None);
        assert!(command.is_none());
        assert!(captured.is_none());
        assert_eq!(response.code, 2);
        assert!(!response.payload.is_empty());
    }

    #[test]
    fn an_unparseable_command_answers_with_claps_own_message() {
        let (command, response, _) = run(&argv(&["nonsuch"]), None);
        assert!(command.is_none(), "there is no command to name");
        assert_eq!(response.code, 2);
        assert_eq!(response.stream, StreamKind::Text);
        let message = String::from_utf8_lossy(&response.payload);
        assert!(message.contains("nonsuch"), "{message}");
    }

    #[test]
    fn a_forwarded_failure_carries_the_same_exit_code_as_a_local_one() {
        // `list displays` needs a backend, which is guarded off by default, so
        // this is a stable failure that does not touch the screen.
        let (command, response, captured) = run(&argv(&["list", "displays"]), None);
        assert!(matches!(command, Some(Command::List(_))));
        assert_ne!(
            response.code, 0,
            "a guarded backend must not report success"
        );
        assert!(
            captured.is_none(),
            "a failed command hands over no full-resolution frame"
        );
    }

    #[test]
    fn a_forwarded_json_failure_is_an_envelope_not_a_sentence() {
        let (_, response, _) = run(&argv(&["--json", "list", "displays"]), None);
        assert_eq!(response.stream, StreamKind::Json);
        let body = String::from_utf8_lossy(&response.payload);
        assert!(body.starts_with('{'), "{body}");
        assert!(body.contains("\"ok\":false"), "{body}");
        assert!(body.contains("\"command\":\"list.displays\""), "{body}");
    }

    #[test]
    fn a_forwarded_success_is_reported_verbatim() {
        // `capture --dry-run` reaches no backend, so it succeeds anywhere.
        let (_, response, captured) = run(&argv(&["capture", "--dry-run"]), None);
        assert_eq!(response.code, 0);
        assert_eq!(response.stream, StreamKind::Text);
        assert!(captured.is_none(), "a dry run takes no pixels");
        let body = String::from_utf8_lossy(&response.payload);
        assert!(body.contains("Would capture"), "{body}");
        assert!(
            !body.ends_with('\n'),
            "the trailing newline is the wire's job"
        );
    }

    #[test]
    fn a_quiet_forwarded_command_says_nothing() {
        let (_, response, _) = run(&argv(&["--quiet", "capture", "--dry-run"]), None);
        assert_eq!(response.code, 0);
        assert!(response.payload.is_empty());
    }

    #[test]
    fn a_json_forwarded_success_is_an_envelope() {
        let (_, response, _) = run(&argv(&["--json", "capture", "--dry-run"]), None);
        assert_eq!(response.stream, StreamKind::Json);
        let body = String::from_utf8_lossy(&response.payload);
        assert!(body.contains("\"ok\":true"), "{body}");
        assert!(body.contains("\"command\":\"capture\""), "{body}");
    }

    #[test]
    fn a_response_survives_the_round_trip_the_client_will_make() {
        // The real check: whatever we produce, `ipc::parse_response` must read
        // back unchanged, or the terminal shows something different from a
        // local run.
        let (_, response, _) = run(&argv(&["--json", "capture", "--dry-run"]), None);
        let wire = ipc::encode_response(&response);
        let parsed = ipc::parse_response(&wire).expect("our own wire format must parse");
        assert_eq!(parsed.code, response.code);
        assert_eq!(parsed.stream, response.stream);
        assert_eq!(parsed.payload, response.payload);
    }

    #[test]
    fn a_forwarded_command_never_changes_the_process_working_directory() {
        let before = std::env::current_dir().expect("a working directory");
        let _ = run(
            &argv(&["capture", "--dry-run", "--output", "relative.png"]),
            Some(&std::env::temp_dir()),
        );
        assert_eq!(
            std::env::current_dir().expect("a working directory"),
            before
        );
    }

    #[test]
    fn forwarded_relative_paths_are_reported_as_the_caller_typed_them() {
        let cwd = std::env::temp_dir().join("scrozz-forwarded-caller");
        let (_, response, _) = run(
            &argv(&["capture", "--dry-run", "--output", "captures/shot.png"]),
            Some(&cwd),
        );
        let body = String::from_utf8_lossy(&response.payload);
        assert!(body.contains("captures/shot.png"), "{body}");
        assert!(!body.contains(&cwd.display().to_string()), "{body}");

        let (_, response, _) = run(
            &argv(&[
                "--json",
                "capture",
                "--dry-run",
                "--output",
                "captures/shot.png",
            ]),
            Some(&cwd),
        );
        let body = String::from_utf8_lossy(&response.payload);
        assert!(body.contains("captures/shot.png"), "{body}");
        assert!(!body.contains(&cwd.display().to_string()), "{body}");
    }

    #[test]
    fn a_request_without_the_direct_policy_is_refused() {
        // The policy field is what says a forwarded command keeps command-line
        // semantics. A request that omits or renames it is from a build that
        // does not share this contract, so it is discarded rather than run
        // under whatever ambient GUI policy happens to be configured.
        let stop = AtomicBool::new(false);
        let honest = ipc::encode_request(&argv(&["capture", "--dry-run"]), None);
        assert!(framed_read(&honest, &stop).is_some());

        let renamed = honest.replace(DIRECT_AFTER_CAPTURE_POLICY, "gui-defaults");
        assert!(framed_read(&renamed, &stop).is_none());

        let absent = honest.replace(
            &format!(",\"after_capture_policy\":\"{DIRECT_AFTER_CAPTURE_POLICY}\""),
            "",
        );
        assert!(framed_read(&absent, &stop).is_none());
    }

    #[test]
    fn an_unsupported_schema_is_refused_before_the_command_is_parsed() {
        let stop = AtomicBool::new(false);
        let honest = ipc::encode_request(&argv(&["capture", "--dry-run"]), None);
        let older = honest.replace(
            &format!("\"schema\":{}", ipc::REQUEST_SCHEMA),
            &format!("\"schema\":{}", ipc::REQUEST_SCHEMA - 1),
        );
        assert!(framed_read(&older, &stop).is_none());
    }

    #[test]
    fn an_excessive_argument_vector_is_refused() {
        let stop = AtomicBool::new(false);
        let many: Vec<String> = (0..5_000).map(|index| format!("--arg{index}")).collect();
        let line = ipc::encode_request(&many, None);
        assert!(framed_read(&line, &stop).is_none());
    }

    /// Frames `line` exactly as a client would and reads it back.
    fn framed_read(
        line: &str,
        stop: &AtomicBool,
    ) -> Option<(Vec<String>, Option<PathBuf>, String)> {
        let mut framed = u32::try_from(line.len())
            .expect("test frame length")
            .to_le_bytes()
            .to_vec();
        framed.extend_from_slice(line.as_bytes());
        read_request(&mut std::io::Cursor::new(framed), stop)
    }

    #[test]
    fn the_worker_waits_for_the_app_to_accept_before_the_client_is_answered() {
        // The interactive path runs on a worker thread, so acceptance has to
        // travel back to the main thread. The reply must not be written until
        // that round trip has completed, or a caller would be told a capture
        // succeeded that the app had not taken.
        let forwarder = Forwarder::start(
            Arc::new(crate::gui::selection::UnsupportedSelector::headless()),
            None,
        )
        .expect("worker starts");
        let (request, response) = request_for(argv(&["capture", "--dry-run"]));
        assert!(forwarder.submit(request));

        let mut admission = loop {
            if let Some(admission) = forwarder.poll() {
                break admission;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        assert!(matches!(admission.command(), Command::Capture(_)));
        assert!(
            admission.take_capture().is_none(),
            "a dry run has no pixels"
        );
        assert!(
            matches!(response.try_recv(), Err(TryRecvError::Empty)),
            "the reply must wait for the app"
        );

        assert!(admission.complete(Ok(())).is_some());
        assert_eq!(
            response
                .recv_timeout(Duration::from_secs(5))
                .expect("a reply after acceptance")
                .code,
            0
        );
    }

    #[test]
    fn an_app_that_refuses_a_worker_command_turns_it_into_the_clients_error() {
        let forwarder = Forwarder::start(
            Arc::new(crate::gui::selection::UnsupportedSelector::headless()),
            None,
        )
        .expect("worker starts");
        let (request, response) = request_for(argv(&["capture", "--dry-run"]));
        assert!(forwarder.submit(request));

        let admission = loop {
            if let Some(admission) = forwarder.poll() {
                break admission;
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        assert!(
            admission
                .complete(Err(CliError::ipc("the capture worker is full")))
                .is_none(),
            "a refused command is not a successful one"
        );

        let response = response
            .recv_timeout(Duration::from_secs(5))
            .expect("a reply after refusal");
        assert_ne!(response.code, 0);
        assert!(String::from_utf8_lossy(&response.payload).contains("the capture worker is full"));
    }

    #[test]
    fn stopping_the_worker_releases_a_command_nobody_will_accept() {
        // Shutdown must not wedge on an admission the main thread has stopped
        // servicing, or quitting would hang for the command timeout.
        let mut forwarder = Forwarder::start(
            Arc::new(crate::gui::selection::UnsupportedSelector::headless()),
            None,
        )
        .expect("worker starts");
        let (request, response) = request_for(argv(&["capture", "--dry-run"]));
        assert!(forwarder.submit(request));
        while forwarder.poll().is_none() {
            std::thread::sleep(Duration::from_millis(2));
        }

        let started = Instant::now();
        forwarder.stop();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "shutdown waited on an admission that was never going to arrive"
        );
        assert_ne!(
            response
                .recv_timeout(Duration::from_secs(1))
                .expect("the client is still answered")
                .code,
            0
        );
    }

    #[cfg(unix)]
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scrozz-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    #[cfg(unix)]
    #[test]
    fn a_server_removes_its_socket_when_dropped() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch("drop");
        let path = dir.join("drop.sock");
        {
            let server = Server::bind_at(path.clone()).expect("binding a fresh path");
            assert!(
                server.path().exists(),
                "the socket should exist while bound"
            );
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(!path.exists(), "the socket should be gone after the drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_stand_in_for_the_private_socket_directory() {
        use std::os::unix::fs::symlink;

        let root = scratch("symlink-parent");
        let real = root.join("real");
        let alias = root.join("alias");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, &alias).unwrap();
        let error = Server::bind_at(alias.join("instance.sock"))
            .err()
            .expect("a symlinked IPC parent must fail closed");
        assert!(error.to_string().contains("not a real directory"));
        let _ = std::fs::remove_file(alias);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_socket_file_is_taken_over() {
        // The residue of a crash: a socket file with nothing behind it. Refusing
        // to start would mean one crash disables the app until someone finds the
        // file and deletes it.
        let dir = scratch("stale");
        let path = dir.join("stale.sock");
        std::fs::write(&path, b"not a socket").expect("writing the residue");

        let server = Server::bind_at(path.clone()).expect("a stale file must not block startup");
        assert_eq!(server.path(), path);
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn polling_an_idle_server_yields_nothing() {
        let dir = scratch("idle");
        let server = Server::bind_at(dir.join("idle.sock")).expect("binding");
        assert!(server.poll().is_none());
        assert!(server.poll().is_none());
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_stalled_local_client_cannot_block_server_shutdown_forever() {
        let dir = scratch("stalled-client");
        let path = dir.join("stalled.sock");
        let server = Server::bind_at(path.clone()).expect("binding");
        let _stalled =
            std::os::unix::net::UnixStream::connect(&path).expect("connect without sending");
        std::thread::sleep(Duration::from_millis(30));

        let started = Instant::now();
        drop(server);
        assert!(
            started.elapsed() < REQUEST_READ_TIMEOUT + Duration::from_secs(1),
            "listener shutdown exceeded its bounded read timeout"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn oversized_forwarded_requests_are_rejected_before_unbounded_allocation() {
        use std::io::Write;
        use std::net::Shutdown;

        let (mut writer, mut reader) =
            std::os::unix::net::UnixStream::pair().expect("local socket pair");
        reader
            .set_read_timeout(Some(REQUEST_READ_POLL))
            .expect("read timeout");
        let sending = std::thread::spawn(move || {
            let announced = u32::try_from(ipc::MAX_REQUEST_BYTES + 1).expect("bounded");
            writer
                .write_all(&announced.to_le_bytes())
                .expect("write oversized frame prefix");
            writer.shutdown(Shutdown::Write).expect("finish request");
        });

        let stop = AtomicBool::new(false);
        assert!(read_request(&mut reader, &stop).is_none());
        sending.join().expect("writer");
    }

    #[cfg(unix)]
    #[test]
    fn a_peer_that_never_finishes_is_bounded_by_the_read_timeout() {
        let (_writer, mut reader) =
            std::os::unix::net::UnixStream::pair().expect("local socket pair");
        reader
            .set_read_timeout(Some(REQUEST_READ_POLL))
            .expect("read timeout");
        let started = Instant::now();
        let stop = AtomicBool::new(false);
        assert!(read_request(&mut reader, &stop).is_none());
        assert!(
            started.elapsed() < REQUEST_READ_TIMEOUT + Duration::from_secs(1),
            "socket read exceeded its configured timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_slow_drip_cannot_restart_the_wall_clock_deadline() {
        use std::io::Write;

        let (mut writer, mut reader) =
            std::os::unix::net::UnixStream::pair().expect("local socket pair");
        reader
            .set_read_timeout(Some(REQUEST_READ_POLL))
            .expect("read timeout");
        let sending = std::thread::spawn(move || {
            for _ in 0..20 {
                if writer.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        let started = Instant::now();
        let stop = AtomicBool::new(false);
        assert!(read_request(&mut reader, &stop).is_none());
        assert!(
            started.elapsed() < REQUEST_READ_TIMEOUT + Duration::from_secs(1),
            "partial reads restarted the wall-clock deadline"
        );
        sending.join().expect("slow writer");
    }

    #[cfg(unix)]
    #[test]
    fn a_second_server_on_a_live_endpoint_is_refused() {
        // This is what stops `scrozz gui` twice producing two menu-bar items.
        let dir = scratch("dup");
        let path = dir.join("dup.sock");

        let first = Server::bind_at(path.clone()).expect("the first should bind");
        let second = Server::bind_at(path.clone());
        assert!(second.is_err(), "the second must be refused");

        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_forwarded_command_reaches_the_server_and_is_answered() {
        // The whole point, end to end: the client half in `ipc` talking to the
        // server half here, over a real socket.
        let _env = crate::test_env::lock();
        let dir = scratch("round-trip");
        let path = dir.join("live.sock");
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&wakes);
        let waker: SurfaceWaker = Arc::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        });
        let server = Server::bind_at_with_waker(path.clone(), Some(waker)).expect("binding");

        // No program name: `try_forward` sends `env::args().skip(1)`, and the
        // server is the side that has to know that.
        let sent = argv(&["capture", "--dry-run"]);
        crate::test_env::set(ipc::ENDPOINT_ENV, &path.to_string_lossy());
        let client = std::thread::spawn(move || ipc::forward(&sent));

        let request = loop {
            if let Some(request) = server.poll() {
                break request;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(request.argv.first().map(String::as_str), Some("capture"));
        assert_eq!(request.after_capture_policy, DIRECT_AFTER_CAPTURE_POLICY);
        assert!(wakes.load(Ordering::Relaxed) > 0);
        let command = request.serve_with(|_, captured| {
            assert!(captured.is_none(), "a dry run has no pixels");
            assert!(
                !client.is_finished(),
                "the caller must remain blocked until the success hook finishes"
            );
            Ok(())
        });
        assert!(matches!(command, Some(Command::Capture(_))));

        let response = client
            .join()
            .expect("the client thread")
            .expect("a well-formed answer");
        assert_eq!(response.code, 0);
        assert!(
            String::from_utf8_lossy(&response.payload).contains("Would capture"),
            "the forwarded output must match a local run"
        );

        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn parked_reply_waits_until_transport_delivery_completes() {
        let (reply, response) = sync_channel(1);
        let (delivered, delivery) = sync_channel(1);
        let request = Request {
            argv: argv(&["record", "--stop"]),
            cwd: None,
            after_capture_policy: DIRECT_AFTER_CAPTURE_POLICY.to_owned(),
            reply,
            delivery: Some(delivery),
        };
        let (finished, completion) = sync_channel(1);
        let worker = std::thread::spawn(move || {
            request.answer_and_wait_delivery(&Ok(Report::new(
                crate::json::Json::obj([("state", crate::json::Json::str("finished"))]),
                "recording finished",
            )));
            let _ = finished.send(());
        });

        let response = response.recv().expect("parked response");
        assert_eq!(response.code, 0);
        assert!(
            completion.try_recv().is_err(),
            "the owner must remain alive until the socket worker delivered the response"
        );
        delivered.send(()).expect("delivery acknowledgment");
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("answer returns after delivery");
        worker.join().expect("reply worker");
    }

    #[test]
    fn a_request_that_finds_the_queue_full_is_told_the_instance_is_busy() {
        use std::io::{Read as _, Write as _};

        // The app queue is exactly one deep. A request that arrives while it is
        // occupied must be refused with a well-formed response rather than
        // retained, which is what keeps a burst of terminal invocations from
        // growing memory without limit.
        let (mut client, mut listener_side) =
            std::os::unix::net::UnixStream::pair().expect("local socket pair");
        listener_side
            .set_read_timeout(Some(REQUEST_READ_POLL))
            .expect("read timeout");

        let (requests, _held) = sync_channel(REQUEST_QUEUE_DEPTH);
        let (occupying_reply, _occupying) = sync_channel(1);
        requests
            .try_send(Request {
                argv: argv(&["capture", "--dry-run"]),
                cwd: None,
                after_capture_policy: DIRECT_AFTER_CAPTURE_POLICY.to_owned(),
                reply: occupying_reply,
                delivery: None,
            })
            .expect("the queue accepts exactly one");

        let stop = Arc::new(AtomicBool::new(false));
        let serving = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || serve_connection(&mut listener_side, &requests, &stop, None)
        });

        let line = ipc::encode_request(&argv(&["capture", "--dry-run"]), None);
        client
            .write_all(
                &u32::try_from(line.len())
                    .expect("request length")
                    .to_le_bytes(),
            )
            .and_then(|()| client.write_all(line.as_bytes()))
            .expect("framed request");

        let mut prefix = [0_u8; 4];
        client.read_exact(&mut prefix).expect("response length");
        let mut payload = vec![0_u8; u32::from_le_bytes(prefix) as usize];
        client.read_exact(&mut payload).expect("response payload");
        // The server waits for this before releasing the connection.
        let ack = b"SCROZZ/2 ACK";
        client
            .write_all(&u32::try_from(ack.len()).expect("ack length").to_le_bytes())
            .and_then(|()| client.write_all(ack))
            .expect("acknowledgement");
        serving.join().expect("listener thread");

        let response = ipc::parse_response(&payload).expect("a well-formed refusal");
        assert_ne!(response.code, 0);
        let body = String::from_utf8_lossy(&response.payload);
        assert!(body.contains("busy"), "{body}");
    }

    #[cfg(unix)]
    #[test]
    fn a_client_may_pause_after_accept_before_sending_its_request() {
        use std::io::Write;

        let dir = scratch("delayed-write");
        let path = dir.join("delayed.sock");
        let server = Server::bind_at(path.clone()).expect("binding");
        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let mut stream = std::os::unix::net::UnixStream::connect(path).expect("connect");
                std::thread::sleep(Duration::from_millis(50));
                let request = ipc::encode_request(&argv(&["capture", "--dry-run"]), None);
                stream
                    .write_all(
                        &u32::try_from(request.len())
                            .expect("request length")
                            .to_le_bytes(),
                    )
                    .and_then(|()| stream.write_all(request.as_bytes()))
                    .expect("delayed framed write");
            }
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        let request = loop {
            if let Some(request) = server.poll() {
                break request;
            }
            assert!(
                Instant::now() < deadline,
                "delayed request never reached the listener"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(request.argv, argv(&["capture", "--dry-run"]));
        drop(request);
        client.join().expect("client");
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
