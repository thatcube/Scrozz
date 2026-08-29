//! What each subcommand does.
//!
//! Every handler returns a [`Report`] or a [`CliError`]; none of them writes to
//! a stream. That separation is what makes the output contract testable — the
//! shape of `--json` is a value these functions return, not a side effect of
//! having run them.
//!
//! # Reading this file while the backends are unfinished
//!
//! Three commands do their whole job today: `settings`, `hotkey` and the
//! `--dry-run` half of `capture`/`record`. The rest resolve their arguments
//! completely, then stop at [`crate::platform`] with a
//! [`CliError::NotImplemented`] naming the crate that owes the work.
//!
//! That order is deliberate. Validation, target resolution and destination
//! selection happen *before* the missing piece, so a mistake in the command line
//! is reported as a mistake in the command line even now, and the resolution
//! logic is exercised by tests that never touch a screen.

use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use scrozz_core::{
    Capture, CaptureRequest, CaptureTarget, CursorMode, Error as CoreError, Frame, Provenance,
    SelectionOptions, SelectionOutcome,
};
use scrozz_export::{Clipboard, Encoder, FrameEncoder, ImageFormat};
use scrozz_ocr::Ocr as _;
use scrozz_record::{
    Recording, RecordingCompletion, RecordingRequest, RecordingSession, RecordingState,
    Salvageability, SessionEvent,
};
use scrozz_store::{
    CaptureId, CaptureRecord, History as _, ImageState, NewRecording, Page, SearchQuery,
    Store as _, VideoCompletion, VideoMetadata, VideoSalvageability,
};

use crate::{
    cli::{
        CaptureArgs, Command, Compositor, DisplaySelector, HistoryCommand, HotkeyCommand,
        InteractiveMode, ListWhat, OcrSubject, RecordArgs, RecordControl, SettingsCommand, Sink,
        TargetSpec,
    },
    fault::{CliError, CliResult},
    gui::selection::CaptureSelector,
    hotkey_config, ipc,
    json::Json,
    platform,
    report::Report,
    settings,
};

const CANCELLATION_POLL: Duration = Duration::from_millis(20);

/// Deadline and cancellation state shared with a forwarded command's transport.
#[derive(Clone, Debug)]
pub(crate) struct ExecutionControl {
    deadline: Option<Instant>,
    cancelled: Option<Arc<AtomicBool>>,
}

impl ExecutionControl {
    pub(crate) const fn local() -> Self {
        Self {
            deadline: None,
            cancelled: None,
        }
    }

    pub(crate) fn forwarded(deadline: Option<Instant>) -> Self {
        Self {
            deadline,
            cancelled: Some(Arc::new(AtomicBool::new(false))),
        }
    }

    pub(crate) fn cancel(&self) {
        if let Some(cancelled) = &self.cancelled {
            cancelled.store(true, Ordering::Release);
        }
    }

    pub(crate) fn check(&self) -> CliResult<()> {
        if self
            .cancelled
            .as_ref()
            .is_some_and(|cancelled| cancelled.load(Ordering::Acquire))
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(CliError::ipc(
                "the forwarded command expired before it could complete",
            ));
        }
        Ok(())
    }

    fn wait(&self, duration: Duration) -> CliResult<()> {
        let wake = Instant::now()
            .checked_add(duration)
            .ok_or_else(|| CliError::usage("the capture delay is too large"))?;
        if self.deadline.is_some_and(|deadline| wake >= deadline) {
            return Err(CliError::ipc(
                "the capture delay exceeds the forwarded-command deadline",
            ));
        }
        if self.deadline.is_none() {
            std::thread::sleep(duration);
            return Ok(());
        }
        loop {
            self.check()?;
            let remaining = wake.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            std::thread::sleep(remaining.min(CANCELLATION_POLL));
        }
    }
}

/// Runs a command locally.
///
/// # Errors
///
/// Whatever the command produces. Cancellation arrives here as
/// [`scrozz_core::Error::Cancelled`] and is rendered as an outcome, not a fault.
pub fn dispatch(command: &Command) -> CliResult<Report> {
    dispatch_controlled(command, &ExecutionControl::local())
}

pub(crate) fn dispatch_controlled(
    command: &Command,
    execution: &ExecutionControl,
) -> CliResult<Report> {
    let mut recording = RecordingManager::default();
    dispatch_inner(command, &mut recording, None, execution)
}

/// Runs a command with access to the recording owned by this process.
pub fn dispatch_with_recording(
    command: &Command,
    recording: &mut RecordingManager,
) -> CliResult<Report> {
    dispatch_inner(command, recording, None, &ExecutionControl::local())
}

/// Runs a command with an existing-loop selector supplied by the GUI.
///
/// Forwarded interactive captures use this entry point from a worker thread, so
/// the synchronous selector contract can wait while the main eframe loop paints
/// and handles input.
pub fn dispatch_with_selector(
    command: &Command,
    selector: &dyn CaptureSelector,
) -> CliResult<Report> {
    let mut recording = RecordingManager::default();
    dispatch_inner(
        command,
        &mut recording,
        Some(selector),
        &ExecutionControl::local(),
    )
}

pub(crate) fn dispatch_with_selector_control(
    command: &Command,
    selector: &dyn CaptureSelector,
    execution: &ExecutionControl,
) -> CliResult<Report> {
    let mut recording = RecordingManager::default();
    dispatch_inner(command, &mut recording, Some(selector), execution)
}

fn dispatch_inner(
    command: &Command,
    recording: &mut RecordingManager,
    selector: Option<&dyn CaptureSelector>,
    execution: &ExecutionControl,
) -> CliResult<Report> {
    with_platform_apartment(|| {
        execution.check()?;
        let report = match command {
            Command::Capture(args) => capture(args, selector, execution),
            Command::Record(args) => record(args, recording, selector, execution),
            Command::List(args) => list(args.what),
            Command::History(args) => history(&args.command),
            Command::Ocr(args) => ocr(args),
            Command::Settings(args) => settings_command(&args.command),
            Command::Hotkey(args) => hotkey(&args.command),
            Command::Gui => gui(),
        }?;
        execution.check()?;
        Ok(report)
    })
}

#[cfg(target_os = "windows")]
fn with_platform_apartment<T>(body: impl FnOnce() -> CliResult<T>) -> CliResult<T> {
    let apartment = scrozz_shell::windows::apartment::Apartment::enter_multithreaded()
        .map_err(CliError::Core)?;
    let result = body();
    drop(apartment);
    result
}

#[cfg(not(target_os = "windows"))]
fn with_platform_apartment<T>(body: impl FnOnce() -> CliResult<T>) -> CliResult<T> {
    body()
}

/// The native recording session owned by one Scrozz process.
#[derive(Default)]
pub struct RecordingManager {
    session: Option<Box<dyn RecordingSession>>,
    phase: Option<ManagedRecordingPhase>,
    destination: Option<PathBuf>,
    target: Option<CaptureTarget>,
    completion: Option<ManagedCompletion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedRecordingPhase {
    Recording,
    Paused,
}

enum ManagedCompletion {
    Finished(Report),
    Failed(CliError),
}

impl RecordingManager {
    /// Whether this process currently owns a recording.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.session.is_some()
    }

    /// Native pause-free media elapsed time.
    #[must_use]
    pub fn elapsed_secs(&self) -> Option<f64> {
        self.session
            .as_ref()
            .and_then(|session| session.engine_elapsed_secs())
    }

    fn start(
        &mut self,
        session: Box<dyn RecordingSession>,
        destination: PathBuf,
        target: CaptureTarget,
        plan: Json,
    ) -> CliResult<Report> {
        if self.is_active() {
            return Err(recording_already_active_error());
        }
        if self.completion.is_some() {
            return Err(CliError::Core(CoreError::InvalidRequest(
                "the previous recording outcome has not been delivered yet".to_owned(),
            )));
        }
        self.session = Some(session);
        self.phase = Some(ManagedRecordingPhase::Recording);
        self.destination = Some(destination.clone());
        self.target = Some(target);
        self.completion = None;
        Ok(Report::new(
            Json::obj([
                ("state", Json::str("recording")),
                ("path", path_json(&destination)),
                ("plan", plan),
            ]),
            format!("Recording to {}.", destination.display()),
        ))
    }

    /// Pauses the live session after native acknowledgement.
    pub fn pause(&mut self) -> CliResult<Report> {
        match self.phase {
            None => return Err(no_recording_error("pause")),
            Some(ManagedRecordingPhase::Paused) => {
                return Err(CliError::Core(CoreError::InvalidRequest(
                    "the recording is already paused".into(),
                )));
            }
            Some(ManagedRecordingPhase::Recording) => {}
        }
        self.session
            .as_mut()
            .expect("a managed recording phase always has a session")
            .pause()?;
        self.phase = Some(ManagedRecordingPhase::Paused);
        Ok(self.state_report("paused", "Recording paused."))
    }

    /// Resumes the live session after native acknowledgement.
    pub fn resume(&mut self) -> CliResult<Report> {
        match self.phase {
            None => return Err(no_recording_error("resume")),
            Some(ManagedRecordingPhase::Recording) => {
                return Err(CliError::Core(CoreError::InvalidRequest(
                    "the recording is not paused".into(),
                )));
            }
            Some(ManagedRecordingPhase::Paused) => {}
        }
        self.session
            .as_mut()
            .expect("a managed recording phase always has a session")
            .resume()?;
        self.phase = Some(ManagedRecordingPhase::Recording);
        Ok(self.state_report("recording", "Recording resumed."))
    }

    /// Stops and finalises the live session.
    pub fn stop(&mut self) -> CliResult<Report> {
        let session = self
            .session
            .take()
            .ok_or_else(|| no_recording_error("stop"))?;
        let target = self.target.clone();
        self.clear_live_state();
        match session.stop() {
            Ok(recording) => match finish_recording_report(recording, target.as_ref()) {
                Ok(report) => {
                    self.completion = Some(ManagedCompletion::Finished(report.clone()));
                    Ok(report)
                }
                Err(error) => {
                    let (stored, returned) = error.shared_pair();
                    self.completion = Some(ManagedCompletion::Failed(stored));
                    Err(returned)
                }
            },
            Err(error) => {
                let (stored, returned) = CliError::Core(error).shared_pair();
                self.completion = Some(ManagedCompletion::Failed(stored));
                Err(returned)
            }
        }
    }

    /// Polls native session events and logically consumes a terminal event.
    pub fn poll(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        match session.poll() {
            Some(SessionEvent::FirstFrame) => {
                tracing::debug!("native recorder accepted its first frame");
            }
            Some(SessionEvent::Warning(message)) => {
                tracing::warn!("native recording warning: {message}");
            }
            Some(SessionEvent::Finished(recording)) => {
                let target = self.target.clone();
                let completion = match finish_recording_report(recording, target.as_ref()) {
                    Ok(report) => ManagedCompletion::Finished(report),
                    Err(error) => ManagedCompletion::Failed(error),
                };
                self.session = None;
                self.clear_live_state();
                self.completion = Some(completion);
            }
            Some(SessionEvent::Failed(error)) => {
                self.session = None;
                self.clear_live_state();
                self.completion = Some(ManagedCompletion::Failed(CliError::from(error)));
            }
            None => {
                let stopped = self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.state() == RecordingState::Stopped);
                if stopped {
                    let _ = self.stop();
                }
            }
        }
    }

    /// Takes the one terminal outcome after stop or asynchronous completion.
    pub fn take_completion(&mut self) -> Option<CliResult<Report>> {
        self.completion.take().map(|completion| match completion {
            ManagedCompletion::Finished(report) => Ok(report),
            ManagedCompletion::Failed(error) => Err(error),
        })
    }

    /// Stops a live session while its owner is shutting down.
    pub fn shut_down(&mut self) -> Option<CliResult<Report>> {
        self.is_active().then(|| self.stop())
    }

    fn state_report(&self, state: &'static str, human: &'static str) -> Report {
        Report::new(
            Json::obj([
                ("state", Json::str(state)),
                ("path", Json::opt(self.destination.as_deref(), path_json)),
                ("elapsed_secs", Json::opt(self.elapsed_secs(), Json::Float)),
            ]),
            human,
        )
    }

    fn clear_live_state(&mut self) {
        self.phase = None;
        self.destination = None;
        self.target = None;
    }
}

impl Drop for RecordingManager {
    fn drop(&mut self) {
        if let Some(Err(error)) = self.shut_down() {
            tracing::error!("could not finalise the recording during shutdown: {error}");
        }
    }
}

/// Runs a foreground recording until a relayed stop, asynchronous terminal
/// event, or Ctrl-C finalises it.
pub fn run_owned_recording(command: &Command, ipc_enabled: bool) -> CliResult<Report> {
    let Command::Record(args) = command else {
        return dispatch(command);
    };
    if args.control().is_some() || args.dry_run {
        return dispatch(command);
    }

    let server = if ipc_enabled {
        Some(crate::gui::server::Server::bind()?)
    } else {
        None
    };
    let interrupted = Arc::new(AtomicBool::new(false));
    ctrlc::set_handler({
        let interrupted = Arc::clone(&interrupted);
        move || interrupted.store(true, Ordering::Release)
    })
    .map_err(|error| {
        CliError::Core(CoreError::Platform(format!(
            "could not install the recording stop handler: {error}"
        )))
    })?;

    let mut recording = RecordingManager::default();
    let _started = dispatch_with_recording(command, &mut recording)?;
    while recording.is_active() {
        if interrupted.swap(false, Ordering::AcqRel) {
            return recording.stop();
        }
        recording.poll();
        if !recording.is_active() {
            break;
        }
        if let Some(server) = &server {
            while let Some(request) = server.poll() {
                request.serve_with_recording(&mut recording);
                if !recording.is_active() {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    recording.take_completion().ok_or_else(|| {
        CliError::Core(CoreError::Platform(
            "the recording ended without a completion report".into(),
        ))
    })?
}

// ---------------------------------------------------------------------------
// capture
// ---------------------------------------------------------------------------

fn capture(
    args: &CaptureArgs,
    selector: Option<&dyn CaptureSelector>,
    execution: &ExecutionControl,
) -> CliResult<Report> {
    execution.check()?;
    args.validate()?;
    let requested_target = args.target.resolve()?;
    let sinks = args.sinks();
    let selection = args.selection_options(None)?;

    let plan = Json::obj([
        ("target", target_json(&requested_target)),
        ("interactive", Json::Bool(args.target.is_interactive())),
        (
            "selection",
            Json::opt(selection.as_ref(), |options| {
                selection_json(options, args.retake)
            }),
        ),
        ("cursor", Json::Bool(args.cursor)),
        ("window_shadow", Json::Bool(!args.no_window_shadow)),
        ("format", Json::str(args.format().slug())),
        ("quality", Json::opt(args.quality, |q| Json::Int(q.into()))),
        ("delay_secs", Json::opt(args.delay, Json::Float)),
        ("sinks", Json::arr(sinks.iter().map(sink_json))),
    ]);

    if args.dry_run {
        return Ok(Report::new(
            Json::obj([("dry_run", Json::Bool(true)), ("plan", plan)]),
            describe_plan("Would capture", &requested_target, &sinks),
        ));
    }

    // Check before interactive preparation: freezing or magnifying the desktop
    // reaches the capture backend too, and must obey the same unstable-backend
    // policy as the final frame.
    platform::ensure_capture_backend_ready()?;

    // The delay is deliberately *not* honoured before the backend check. Making
    // a user wait five seconds to be told the feature is unimplemented is a
    // small cruelty that costs nothing to avoid.
    let backend = platform::capture_backend()?;
    let mut lifecycle = SelectorLifecycle::new(selector);
    let (target, selection_outcome, frozen_capture) = match requested_target {
        TargetSpec::Interactive(_) => {
            let remembered = if args.retake {
                let remembered = crate::selection_store::RememberedRegionStore::default_location()?
                    .load()?
                    .ok_or_else(|| {
                        CliError::usage(
                            "--retake needs a previous region, but no region has been captured yet",
                        )
                    })?;
                let displays = backend.displays()?;
                Some((remembered.rect, remembered.display_for(&displays)))
            } else {
                None
            };
            let options = args
                .selection_options(remembered)?
                .expect("an interactive target has selection options");
            let (outcome, frozen) = select_target(&options, args, selector)?;
            (outcome.target.clone(), Some(outcome), frozen)
        }
        concrete => (capture_target(&concrete)?, None, None),
    };
    execution.check()?;

    let request = CaptureRequest {
        target,
        cursor: if args.cursor {
            CursorMode::Visible
        } else {
            CursorMode::Hidden
        },
        include_window_shadow: !args.no_window_shadow,
    };

    if selection_outcome.is_none()
        && let Some(secs) = args.delay
    {
        let delay = Duration::try_from_secs_f64(secs)
            .map_err(|_| CliError::usage("the capture delay is too large"))?;
        execution.wait(delay)?;
    }
    if selection_outcome.is_none()
        && let Some(selector) = selector
    {
        selector.begin_capture()?;
    }

    let backend_name = backend.name().to_owned();
    let capture = match frozen_capture {
        Some(capture) => capture,
        None => crate::gui::selection::capture_selected(
            backend.as_ref(),
            &request,
            selection_outcome.as_ref(),
        )?,
    };
    execution.check()?;
    lifecycle.finish();
    if let Some(outcome) = selection_outcome.as_ref() {
        remember_selection(outcome, backend.as_ref());
    }
    let frame = &capture.frame;

    let bytes = FrameEncoder::new()
        .encode(frame, args.format().to_export())
        .map_err(CliError::Core)?;
    execution.check()?;

    let mut written = Vec::new();
    let mut raw = None;
    for sink in &sinks {
        execution.check()?;
        match sink {
            Sink::File(path) => {
                std::fs::write(path, &bytes).map_err(|e| CliError::Core(CoreError::Io(e)))?;
                written.push(path.display().to_string());
            }
            Sink::Clipboard => {
                scrozz_export::SystemClipboard::new()
                    .write_image(frame)
                    .map_err(CliError::Core)?;
                written.push("clipboard".to_string());
            }
            Sink::Stdout => raw = Some(bytes.clone()),
            // D18: any folder the user picks, which is what lets a Dropbox or
            // iCloud directory provide sync for free with no service on our side.
            Sink::DefaultFolder => {
                let path = crate::output::export_default(&bytes)?;
                written.push(path.display().to_string());
            }
        }
    }
    execution.check()?;

    let data = Json::obj([
        ("plan", plan),
        (
            "selection_result",
            Json::opt(selection_outcome.as_ref(), selection_outcome_json),
        ),
        ("width", Json::Int(i64::from(frame.width()))),
        ("height", Json::Int(i64::from(frame.height()))),
        ("scale", Json::Float(frame.scale.get())),
        ("bytes", Json::Int(bytes.len() as i64)),
        ("backend", Json::str(backend_name)),
        ("provenance", Json::str(format!("{:?}", capture.provenance))),
        (
            "written",
            Json::arr(written.iter().map(|w| Json::str(w.as_str()))),
        ),
    ]);

    let human = format!(
        "Captured {}×{} at {}× ({} KB){}",
        frame.width(),
        frame.height(),
        frame.scale.get(),
        bytes.len() / 1024,
        if written.is_empty() {
            String::new()
        } else {
            format!(" → {}", written.join(", "))
        }
    );

    let mut report = Report::new(data, human);
    report.raw = raw;
    Ok(report)
}

struct SelectorLifecycle<'a> {
    selector: Option<&'a dyn CaptureSelector>,
    active: bool,
}

impl<'a> SelectorLifecycle<'a> {
    fn new(selector: Option<&'a dyn CaptureSelector>) -> Self {
        Self {
            selector,
            active: selector.is_some(),
        }
    }

    fn finish(&mut self) {
        if self.active {
            if let Some(selector) = self.selector {
                selector.capture_finished();
            }
            self.active = false;
        }
    }
}

impl Drop for SelectorLifecycle<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

fn select_target(
    options: &SelectionOptions,
    args: &CaptureArgs,
    selector: Option<&dyn CaptureSelector>,
) -> CliResult<(SelectionOutcome, Option<Capture>)> {
    let cursor = if args.cursor {
        CursorMode::Visible
    } else {
        CursorMode::Hidden
    };
    if let Some(selector) = selector {
        let capabilities = selector.capabilities();
        let downgrades = capabilities.downgrades(options);
        if (args.fixed_size.is_some() && !capabilities.exact_size)
            || (args.aspect.is_some() && !capabilities.aspect_lock)
            || (args.retake && !capabilities.remembered_region)
        {
            return Err(CliError::Core(CoreError::Unsupported {
                what: "the requested interactive selection controls".to_owned(),
                why: format!(
                    "the {} selector cannot provide {}",
                    selector.name(),
                    downgrades.join(", ")
                ),
            }));
        }
        if !downgrades.is_empty() {
            tracing::warn!(
                selector = selector.name(),
                unavailable = %downgrades.join(", "),
                "the platform selector cannot draw every requested aid"
            );
        }
        let outcome = selector.select_for_capture(&capabilities.honour(options), cursor)?;
        let request = CaptureRequest {
            target: outcome.target.clone(),
            cursor,
            include_window_shadow: !args.no_window_shadow,
        };
        let frozen = selector.take_frozen_capture(&request);
        return Ok((outcome, frozen));
    }

    Ok(crate::gui::select_once(
        options,
        cursor,
        !args.no_window_shadow,
    )?)
}

fn remember_selection(outcome: &SelectionOutcome, backend: &dyn scrozz_core::CaptureBackend) {
    if outcome.mode != scrozz_core::SelectionMode::Region {
        return;
    }
    let Some(rect) = outcome.rect else {
        tracing::warn!("a region selector returned no rectangle, so it cannot be remembered");
        return;
    };
    let displays = match backend.displays() {
        Ok(displays) => displays,
        Err(error) => {
            tracing::warn!("could not fingerprint the selected display: {error}");
            Vec::new()
        }
    };
    let display = outcome
        .display
        .as_ref()
        .and_then(|id| displays.iter().find(|display| display.id == *id));
    let remembered = crate::selection_store::RememberedRegion::new(rect, display);
    if let Err(error) = crate::selection_store::RememberedRegionStore::default_location()
        .and_then(|store| store.save(remembered))
    {
        tracing::warn!("the capture succeeded but its region could not be remembered: {error}");
    }
}

fn selection_outcome_json(outcome: &SelectionOutcome) -> Json {
    Json::obj([
        ("mode", Json::str(outcome.mode.slug())),
        ("source", Json::str(outcome.source.slug())),
        (
            "rect",
            Json::opt(outcome.rect, |rect| {
                Json::obj([
                    ("x", Json::Float(rect.origin.x)),
                    ("y", Json::Float(rect.origin.y)),
                    ("width", Json::Float(rect.size.width)),
                    ("height", Json::Float(rect.size.height)),
                ])
            }),
        ),
        (
            "display",
            Json::opt(outcome.display.as_ref(), |display| {
                Json::str(display.0.as_str())
            }),
        ),
        ("scale", Json::Float(outcome.scale.get())),
    ])
}

/// Turns a resolved [`TargetSpec`] into the core request type.
///
/// The interactive modes have no representation here on purpose: choosing a
/// target on screen is the overlay's job, and it hands back a concrete target.
/// Modelling "the user has not chosen yet" as a [`CaptureTarget`] would push
/// that uncertainty into every backend.
fn capture_target(spec: &TargetSpec) -> CliResult<CaptureTarget> {
    match spec {
        TargetSpec::Region(rect) => Ok(CaptureTarget::Region(*rect)),
        TargetSpec::AllDisplays => Ok(CaptureTarget::AllDisplays),
        // Resolving a name needs enumeration, so it goes through the same
        // backend the capture will use — an id resolved by a different object
        // is an id that can disagree.
        TargetSpec::Display(sel) => {
            let displays = platform::target_enumerator()?.displays()?;
            let found = match sel {
                DisplaySelector::Primary => displays.iter().find(|d| d.is_primary),
                // The pointer's display, which is where an overlay should appear.
                DisplaySelector::Active => platform::target_enumerator()
                    .ok()
                    .and_then(|e| e.active_display().ok())
                    .and_then(|a| displays.iter().find(|d| d.id == a.id))
                    .or_else(|| displays.iter().find(|d| d.is_primary)),
                DisplaySelector::Id(name) => displays
                    .iter()
                    .find(|d| d.id.0 == *name || d.name.eq_ignore_ascii_case(name)),
            };
            found
                .map(|d| CaptureTarget::Display(d.id.clone()))
                .ok_or_else(|| {
                    CliError::Core(CoreError::InvalidRequest(format!(
                        "no display matches {sel:?}; `scrozz list displays` shows what is available"
                    )))
                })
        }
        TargetSpec::Window(name) => {
            let windows = platform::target_enumerator()?.windows()?;
            windows
                .iter()
                .find(|w| {
                    w.id.0 == *name
                        || w.title
                            .as_deref()
                            .is_some_and(|t| t.to_lowercase().contains(&name.to_lowercase()))
                })
                .map(|w| CaptureTarget::Window(w.id.clone()))
                .ok_or_else(|| {
                    CliError::Core(CoreError::InvalidRequest(format!(
                        "no window matches {name:?}; `scrozz list windows` shows what is available"
                    )))
                })
        }
        TargetSpec::Interactive(_) => Err(CliError::not_implemented(
            "choosing a target on screen",
            "scrozz-ui (the selection overlay)",
        )),
    }
}

// ---------------------------------------------------------------------------
// record
// ---------------------------------------------------------------------------

/// A resolved recording request ready for either the CLI owner or GUI machine.
pub(crate) struct PreparedRecording {
    pub(crate) request: RecordingRequest,
    pub(crate) destination: PathBuf,
    plan: Json,
}

impl PreparedRecording {
    pub(crate) fn started_report(&self) -> Report {
        Report::new(
            Json::obj([
                ("state", Json::str("recording")),
                ("path", path_json(&self.destination)),
                ("plan", self.plan.clone()),
            ]),
            format!("Recording to {}.", self.destination.display()),
        )
    }
}

fn record(
    args: &RecordArgs,
    recording: &mut RecordingManager,
    selector: Option<&dyn CaptureSelector>,
    execution: &ExecutionControl,
) -> CliResult<Report> {
    if let Some(control) = args.control() {
        return match control {
            RecordControl::Stop => recording.stop(),
            RecordControl::Pause => recording.pause(),
            RecordControl::Resume => recording.resume(),
        };
    }

    let requested_target = recording_target_spec(args)?;
    let plan = recording_plan(args, &requested_target);

    if args.dry_run {
        return Ok(Report::new(
            Json::obj([("dry_run", Json::Bool(true)), ("plan", plan)]),
            format!(
                "Would record {} at {} fps.",
                describe_target(&requested_target),
                args.fps
            ),
        ));
    }

    if recording.is_active() {
        return Err(recording_already_active_error());
    }
    let target = resolve_recording_capture_target(&requested_target, selector)?;
    execution.check()?;
    let resolved_plan = recording_plan_for_target(args, &target);
    let prepared = prepare_recording(args, target, resolved_plan)?;
    let session = platform::start_recording(&prepared.request)?;
    recording.start(
        session,
        prepared.destination,
        prepared.request.target,
        prepared.plan,
    )
}

pub(crate) fn prepare_recording_args(args: &RecordArgs) -> CliResult<PreparedRecording> {
    let target = recording_target_spec(args)?;
    let concrete = capture_target(&target)?;
    let plan = recording_plan_for_target(args, &concrete);
    prepare_recording(args, concrete, plan)
}

pub(crate) fn prepare_recording_args_for_target(
    args: &RecordArgs,
    target: CaptureTarget,
) -> CliResult<PreparedRecording> {
    let requested = recording_target_spec(args)?;
    if !matches!(requested, TargetSpec::Interactive(_)) {
        return Err(CliError::Core(CoreError::InvalidRequest(
            "a caller-supplied recording target is only valid for an interactive request"
                .to_owned(),
        )));
    }
    let plan = recording_plan_for_target(args, &target);
    prepare_recording(args, target, plan)
}

fn prepare_recording(
    args: &RecordArgs,
    target: CaptureTarget,
    plan: Json,
) -> CliResult<PreparedRecording> {
    let destination = absolute_recording_path(match &args.output {
        Some(path) => path.clone(),
        None => crate::output::default_recording_path()?,
    })?;
    let mut request = RecordingRequest::new(target);
    request.destination = Some(destination.clone());
    request.microphone = args.microphone;
    request.system_audio = args.system_audio;
    request.fps = args.fps;
    request.show_cursor = args.cursor;
    request.quality = args.quality.to_recording();
    request.resolution = args.resolution.to_recording();
    request.video_codec = args.codec.to_recording();
    Ok(PreparedRecording {
        request,
        destination,
        plan,
    })
}

pub(crate) fn recording_target_spec(args: &RecordArgs) -> CliResult<TargetSpec> {
    if args.target.region.is_none()
        && args.target.window.is_none()
        && args.target.display.is_none()
        && !args.target.all_displays
        && args.target.interactive.is_none()
    {
        Ok(TargetSpec::Display(DisplaySelector::Active))
    } else {
        Ok(args.target.resolve()?)
    }
}

fn recording_plan(args: &RecordArgs, target: &TargetSpec) -> Json {
    recording_plan_with_target(args, target_json(target))
}

fn recording_plan_for_target(args: &RecordArgs, target: &CaptureTarget) -> Json {
    recording_plan_with_target(args, capture_target_json(target))
}

fn recording_plan_with_target(args: &RecordArgs, target: Json) -> Json {
    Json::obj([
        ("target", target),
        ("fps", Json::Int(args.fps.into())),
        ("quality", Json::str(args.quality.to_recording().slug())),
        ("resolution", Json::str(args.resolution.slug())),
        ("codec", Json::str(args.codec.slug())),
        ("microphone", Json::Bool(args.microphone)),
        ("system_audio", Json::Bool(args.system_audio)),
        ("cursor", Json::Bool(args.cursor)),
        ("output", Json::opt(args.output.as_deref(), path_json)),
    ])
}

pub(crate) fn select_recording_target(
    mode: InteractiveMode,
    selector: Option<&dyn CaptureSelector>,
) -> CliResult<CaptureTarget> {
    select_recording_target_with_memory(mode, selector, true)
}

pub(crate) fn select_recording_target_with_memory(
    mode: InteractiveMode,
    selector: Option<&dyn CaptureSelector>,
    remember_region: bool,
) -> CliResult<CaptureTarget> {
    let options = recording_selection_options(mode, remember_region)?;
    let client_selector_available = selector
        .map(|selector| selector.capabilities())
        .is_some_and(|capabilities| capabilities.supports(options.mode));
    if !client_selector_available && let Some(target) = compositor_recording_target(mode) {
        return target;
    }
    let mut lifecycle = SelectorLifecycle::new(selector);
    let outcome = if let Some(selector) = selector {
        let capabilities = selector.capabilities();
        if !capabilities.supports(options.mode) {
            return Err(CliError::Core(CoreError::Unsupported {
                what: format!("interactive {} recording selection", interactive_slug(mode)),
                why: format!(
                    "the {} selector does not support {} mode",
                    selector.name(),
                    options.mode.label()
                ),
            }));
        }
        selector.select(&capabilities.honour(&options))?
    } else {
        crate::gui::select_once(&options, CursorMode::Hidden, false)?.0
    };
    lifecycle.finish();
    if remember_region && outcome.mode == scrozz_core::SelectionMode::Region {
        match platform::capture_backend() {
            Ok(backend) => remember_selection(&outcome, backend.as_ref()),
            Err(error) => {
                tracing::warn!(
                    "recording target was selected but could not be remembered: {error}"
                );
            }
        }
    }
    Ok(outcome.target)
}

fn recording_selection_options(
    mode: InteractiveMode,
    remember_region: bool,
) -> CliResult<SelectionOptions> {
    let mut options = SelectionOptions::for_mode(mode.initial_mode());
    options.hud = mode.shows_hud();
    if !remember_region || !matches!(mode, InteractiveMode::Region | InteractiveMode::AllInOne) {
        return Ok(options);
    }
    let Some(remembered) =
        crate::selection_store::RememberedRegionStore::default_location()?.load()?
    else {
        return Ok(options);
    };
    let displays = platform::target_enumerator()?.displays()?;
    options.remembered_display = remembered.display_for(&displays);
    options.remembered = Some(remembered.rect);
    Ok(options)
}

#[cfg(target_os = "linux")]
fn compositor_recording_target(mode: InteractiveMode) -> Option<CliResult<CaptureTarget>> {
    use scrozz_shell::{DisplayServer, Session};

    if Session::detect().server != DisplayServer::Wayland {
        return None;
    }
    Some(match mode {
        InteractiveMode::Window => Ok(CaptureTarget::Window(scrozz_core::WindowId(
            "portal:interactive-window".to_owned(),
        ))),
        InteractiveMode::Display => Ok(CaptureTarget::Display(scrozz_core::DisplayId(
            "portal:interactive-display".to_owned(),
        ))),
        InteractiveMode::Region => Err(CliError::Core(CoreError::Unsupported {
            what: "interactive recording region selection on Wayland".to_owned(),
            why: "the ScreenCast portal can choose a monitor or window but does not return \
                  region coordinates; Scrozz will not fabricate capture geometry"
                .to_owned(),
        })),
        InteractiveMode::AllInOne => Err(CliError::Core(CoreError::Unsupported {
            what: "all-in-one recording selection on Wayland".to_owned(),
            why: "the ScreenCast portal requires the source type before opening and cannot \
                  return a client-side all-in-one window/display/region choice"
                .to_owned(),
        })),
    })
}

#[cfg(not(target_os = "linux"))]
const fn compositor_recording_target(_mode: InteractiveMode) -> Option<CliResult<CaptureTarget>> {
    None
}

fn resolve_recording_capture_target(
    target: &TargetSpec,
    selector: Option<&dyn CaptureSelector>,
) -> CliResult<CaptureTarget> {
    match target {
        TargetSpec::Interactive(mode) => select_recording_target(*mode, selector),
        concrete => capture_target(concrete),
    }
}

fn absolute_recording_path(path: PathBuf) -> CliResult<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .map_err(|error| {
                CliError::Core(CoreError::Platform(format!(
                    "could not resolve the recording destination: {error}"
                )))
            })?
            .join(path))
    }
}

/// Starts the GUI's default full-desktop recording.
pub fn start_default_recording(recording: &mut RecordingManager) -> CliResult<Report> {
    if recording.is_active() {
        return Err(recording_already_active_error());
    }
    let destination = crate::output::default_recording_path()?;
    let target = CaptureTarget::AllDisplays;
    let mut request = scrozz_record::RecordingRequest::new(target.clone());
    request.destination = Some(destination.clone());
    let plan = Json::obj([
        ("target", capture_target_json(&target)),
        ("fps", Json::Int(request.fps.into())),
        ("quality", Json::str(request.quality.slug())),
        ("resolution", Json::str(request.resolution.slug())),
        ("codec", Json::str(request.video_codec.slug())),
        ("microphone", Json::Bool(request.microphone)),
        ("system_audio", Json::Bool(request.system_audio)),
        ("cursor", Json::Bool(request.show_cursor)),
        ("output", path_json(&destination)),
    ]);
    let session = platform::start_recording(&request)?;
    recording.start(session, destination, target, plan)
}

fn recording_already_active_error() -> CliError {
    CliError::Core(CoreError::InvalidRequest(
        "a recording is already in progress; stop it before starting another".into(),
    ))
}

fn no_recording_error(action: &str) -> CliError {
    CliError::Core(CoreError::InvalidRequest(format!(
        "no recording is in progress, so there is nothing to {action}"
    )))
}

pub(crate) fn finish_recording_report(
    recording: Recording,
    fallback_target: Option<&CaptureTarget>,
) -> CliResult<Report> {
    recording.require_native()?;
    let (history_id, history_error) = match persist_recording(&recording, fallback_target) {
        Ok(id) => (Some(id), None),
        Err(error) => {
            tracing::warn!("recording was saved but could not enter history: {error}");
            (None, Some(error.to_string()))
        }
    };
    recording_report(recording, history_id.as_ref(), history_error.as_deref())
}

fn persist_recording(
    recording: &Recording,
    fallback_target: Option<&CaptureTarget>,
) -> CliResult<CaptureId> {
    recording.require_native()?;
    let (engine, native_target) = match &recording.provenance {
        scrozz_record::RecordingProvenance::Native { engine, target } => {
            (engine.clone(), target.as_ref())
        }
        scrozz_record::RecordingProvenance::Synthetic { .. } => {
            unreachable!("require_native rejects synthetic output before history construction")
        }
    };
    let target = native_target.or(fallback_target).cloned().ok_or_else(|| {
        CliError::Core(CoreError::Storage(
            "native recording did not report its capture target".into(),
        ))
    })?;
    let provenance = provenance_for_target(&target);
    let completion = match &recording.completion {
        RecordingCompletion::Complete => VideoCompletion::Complete,
        RecordingCompletion::Partial {
            salvageability,
            reason,
        } => VideoCompletion::Partial {
            salvageability: match salvageability {
                Salvageability::InitialisationOnly => VideoSalvageability::InitialisationOnly,
                Salvageability::Playable => VideoSalvageability::Playable,
            },
            reason: reason.clone(),
        },
    };
    let video = VideoMetadata {
        path: recording.path.clone(),
        duration_secs: recording.duration_secs,
        engine,
        completion,
        size: recording.metadata.size,
        frames: recording.metadata.frames,
        audio_channels: recording.metadata.audio_channels,
        file_size_bytes: recording.metadata.file_size_bytes,
        codec: recording
            .metadata
            .video_codec
            .map(|codec| codec.slug().to_owned()),
        quality: recording
            .metadata
            .quality
            .map(|quality| quality.slug().to_owned()),
        resolution: recording
            .metadata
            .resolution
            .map(|resolution| resolution.slug()),
    };
    let mut store = platform::store()?;
    Ok(store.insert_recording(NewRecording::new(target, provenance, video))?)
}

fn provenance_for_target(target: &CaptureTarget) -> Provenance {
    match target {
        CaptureTarget::Display(_) => Provenance::Display,
        CaptureTarget::Window(_) => Provenance::Window,
        CaptureTarget::Region(_) => Provenance::Region,
        CaptureTarget::AllDisplays => Provenance::AllDisplays,
    }
}

fn recording_report(
    recording: Recording,
    history_id: Option<&CaptureId>,
    history_error: Option<&str>,
) -> CliResult<Report> {
    recording.require_native()?;
    let (completion, salvageability, reason) = match &recording.completion {
        RecordingCompletion::Complete => ("complete", "playable", None),
        RecordingCompletion::Partial {
            salvageability,
            reason,
        } => (
            "partial",
            match salvageability {
                Salvageability::InitialisationOnly => "initialisation-only",
                Salvageability::Playable => "playable",
            },
            Some(reason.as_str()),
        ),
    };
    let human = match &recording.completion {
        RecordingCompletion::Complete => format!(
            "Recorded {:.2} seconds to {}.",
            recording.duration_secs,
            recording.path.display()
        ),
        RecordingCompletion::Partial { reason, .. } => format!(
            "Retained a {salvageability} partial recording ({:.2} seconds) at {}: {reason}",
            recording.duration_secs,
            recording.path.display()
        ),
    };
    let (engine, target) = match &recording.provenance {
        scrozz_record::RecordingProvenance::Native { engine, target } => {
            (engine.as_str(), target.as_ref())
        }
        scrozz_record::RecordingProvenance::Synthetic { .. } => {
            unreachable!("require_native rejects synthetic output before report construction")
        }
    };
    let report = Report::new(
        Json::obj([
            ("state", Json::str("stopped")),
            ("media_kind", Json::str("video")),
            ("history_id", Json::opt(history_id, |id| Json::str(&id.0))),
            ("history_error", Json::opt(history_error, Json::str)),
            ("path", path_json(&recording.path)),
            ("duration_secs", Json::Float(recording.duration_secs)),
            ("completion", Json::str(completion)),
            ("salvageability", Json::str(salvageability)),
            ("playable", Json::Bool(recording.is_playable())),
            ("reason", Json::opt(reason, Json::str)),
            ("engine", Json::str(engine)),
            ("target", Json::opt(target, capture_target_json)),
            (
                "width",
                Json::opt(recording.metadata.size, |size| Json::Float(size.width)),
            ),
            (
                "height",
                Json::opt(recording.metadata.size, |size| Json::Float(size.height)),
            ),
            (
                "frames",
                Json::opt(recording.metadata.frames, |frames| Json::Int(frames as i64)),
            ),
            (
                "audio_channels",
                Json::opt(recording.metadata.audio_channels, |channels| {
                    Json::Int(i64::from(channels))
                }),
            ),
            (
                "file_size_bytes",
                Json::opt(recording.metadata.file_size_bytes, |bytes| {
                    Json::Int(bytes as i64)
                }),
            ),
            (
                "codec",
                Json::opt(recording.metadata.video_codec, |codec| {
                    Json::str(codec.slug())
                }),
            ),
            (
                "quality",
                Json::opt(recording.metadata.quality, |quality| {
                    Json::str(quality.slug())
                }),
            ),
            (
                "resolution",
                Json::opt(recording.metadata.resolution, |resolution| {
                    Json::str(resolution.slug())
                }),
            ),
        ]),
        human,
    );
    if completion == "partial" {
        return Err(CliError::partial_recording(
            CoreError::Platform(reason.map_or_else(
                || "recording did not finish cleanly".to_owned(),
                str::to_owned,
            )),
            recording.path.to_string_lossy(),
            recording.is_playable(),
            salvageability,
            recording.duration_secs,
            history_id.map(|id| id.0.clone()),
            history_error.map(str::to_owned),
        ));
    }
    Ok(report)
}

fn capture_target_json(target: &CaptureTarget) -> Json {
    match target {
        CaptureTarget::Region(rect) => Json::obj([
            ("kind", Json::str("region")),
            ("x", Json::Float(rect.origin.x)),
            ("y", Json::Float(rect.origin.y)),
            ("width", Json::Float(rect.size.width)),
            ("height", Json::Float(rect.size.height)),
        ]),
        CaptureTarget::Window(id) => {
            Json::obj([("kind", Json::str("window")), ("id", Json::str(&id.0))])
        }
        CaptureTarget::Display(id) => {
            Json::obj([("kind", Json::str("display")), ("id", Json::str(&id.0))])
        }
        CaptureTarget::AllDisplays => Json::obj([("kind", Json::str("all-displays"))]),
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list(what: ListWhat) -> CliResult<Report> {
    let enumerator = platform::target_enumerator();

    match what {
        ListWhat::Displays => {
            let displays = enumerator?.displays()?;
            let data = Json::arr(displays.iter().map(|d| {
                Json::obj([
                    ("id", Json::str(d.id.0.as_str())),
                    ("name", Json::str(d.name.as_str())),
                    ("width", Json::Float(d.bounds.size.width)),
                    ("height", Json::Float(d.bounds.size.height)),
                    ("scale", Json::Float(d.scale.get())),
                    ("primary", Json::Bool(d.is_primary)),
                ])
            }));
            let human = displays
                .iter()
                .map(|d| {
                    format!(
                        "{}  {}×{} @{}×{}",
                        d.id.0,
                        d.bounds.size.width,
                        d.bounds.size.height,
                        d.scale.get(),
                        if d.is_primary { "  (primary)" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Report::new(data, human))
        }
        ListWhat::Windows => {
            // D8: on Wayland this is not a missing feature, it is a missing
            // protocol. Do not claim that the RegionSelector path can stand in
            // for the portal-owned capture picker: the portal does not return a
            // window id or desktop geometry.
            if is_wayland() {
                return Err(CliError::Core(CoreError::Unsupported {
                    what: "listing windows".to_string(),
                    why: "Wayland has no window enumeration protocol: a client \
                          cannot see other clients' windows, by design. Capture \
                          a display instead; portal-owned window capture and \
                          positioned all-display composition are not yet connected \
                          to this command."
                        .to_string(),
                }));
            }
            let windows = enumerator?.windows()?;
            let data = Json::arr(windows.iter().map(|w| {
                Json::obj([
                    ("id", Json::str(w.id.0.as_str())),
                    ("title", Json::opt(w.title.as_deref(), Json::str)),
                    (
                        "application",
                        Json::opt(w.application.as_deref(), Json::str),
                    ),
                    ("width", Json::Float(w.bounds.size.width)),
                    ("height", Json::Float(w.bounds.size.height)),
                ])
            }));
            let human = windows
                .iter()
                .map(|w| {
                    format!(
                        "{}  {}  {}",
                        w.id.0,
                        w.application.as_deref().unwrap_or("—"),
                        w.title.as_deref().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Report::new(data, human))
        }
    }
}

/// Whether this is a Wayland session.
fn is_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok_and(|v| !v.is_empty())
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

fn history(command: &HistoryCommand) -> CliResult<Report> {
    match command {
        HistoryCommand::List { limit, pinned } => {
            let store = platform::store()?;
            let limit = limit.unwrap_or(50);
            if limit == 0 {
                return Err(CliError::usage("history list limit must be at least one"));
            }
            let page_limit = u32::try_from(limit).map_err(|_| {
                CliError::usage(format!("history list limit {limit} exceeds {}", u32::MAX))
            })?;
            let mut query = SearchQuery::all().paged(Page::new(page_limit, 0));
            if *pinned {
                query = query.pinned_only();
            }
            let records = store.search(&query)?;
            let data = Json::arr(records.iter().map(history_record_json));
            let human = if records.is_empty() {
                "History is empty.".to_owned()
            } else {
                records
                    .iter()
                    .map(history_record_human)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok(Report::new(data, human))
        }
        HistoryCommand::Get { id, output, stdout } => {
            let id = history_id(id)?;
            let mut store = platform::store()?;
            let record = store.record(&id)?.ok_or_else(|| {
                CliError::Core(CoreError::InvalidRequest(format!(
                    "history contains no capture {}",
                    id.0
                )))
            })?;
            let mut report =
                Report::new(history_record_json(&record), history_record_human(&record));
            if *stdout {
                if record.video.is_some() {
                    return Err(CliError::Core(CoreError::InvalidRequest(
                        "video history cannot be written to stdout without buffering the entire recording; use --output for streaming copy"
                            .into(),
                    )));
                }
                let bytes = history_media_bytes(&mut store, &record)?;
                report = report.with_raw(bytes);
            } else if let Some(path) = output {
                if let Some(video) = &record.video {
                    copy_new_file(&video.path, path)?;
                } else {
                    let bytes = history_media_bytes(&mut store, &record)?;
                    write_new_file(path, &bytes)?;
                }
                report.human = format!("Wrote history item {} to {}.", id.0, path.display());
            }
            Ok(report)
        }
        HistoryCommand::Delete { ids } => {
            let mut store = platform::store()?;
            let ids = ids
                .iter()
                .map(|id| history_id(id))
                .collect::<CliResult<Vec<_>>>()?;
            let mut deleted = Vec::new();
            let mut missing = Vec::new();
            for id in ids {
                if store.delete(&id)? {
                    deleted.push(id.0);
                } else {
                    missing.push(id.0);
                }
            }
            Ok(Report::new(
                Json::obj([
                    (
                        "deleted",
                        Json::arr(deleted.iter().map(|id| Json::str(id.as_str()))),
                    ),
                    (
                        "missing",
                        Json::arr(missing.iter().map(|id| Json::str(id.as_str()))),
                    ),
                ]),
                format!(
                    "Deleted {} history item{}; {} not found.",
                    deleted.len(),
                    if deleted.len() == 1 { "" } else { "s" },
                    missing.len()
                ),
            ))
        }
        HistoryCommand::Pin { id, unpin } => {
            let id = history_id(id)?;
            let mut store = platform::store()?;
            store.set_pinned(&id, !*unpin)?;
            Ok(Report::new(
                Json::obj([
                    ("id", Json::str(id.0.as_str())),
                    ("pinned", Json::Bool(!*unpin)),
                ]),
                format!(
                    "{} history item {}.",
                    if *unpin { "Unpinned" } else { "Pinned" },
                    id.0
                ),
            ))
        }
    }
}

fn history_id(value: &str) -> CliResult<CaptureId> {
    if !scrozz_store::id::is_valid_id(value) {
        return Err(CliError::usage(format!(
            "{value:?} is not a valid capture id"
        )));
    }
    Ok(CaptureId(value.to_owned()))
}

fn history_record_json(record: &CaptureRecord) -> Json {
    let video = record.video.as_ref().map(|video| {
        let (completion, salvageability, failure) = match &video.completion {
            VideoCompletion::Complete => ("complete", None, None),
            VideoCompletion::Partial {
                salvageability,
                reason,
            } => (
                "partial",
                Some(match salvageability {
                    VideoSalvageability::InitialisationOnly => "initialisation-only",
                    VideoSalvageability::Playable => "playable",
                }),
                Some(reason.as_str()),
            ),
        };
        Json::obj([
            ("path", Json::str(video.path.to_string_lossy())),
            ("duration_secs", Json::Float(video.duration_secs)),
            ("engine", Json::str(video.engine.as_str())),
            ("completion", Json::str(completion)),
            ("salvageability", Json::opt(salvageability, Json::str)),
            ("failure", Json::opt(failure, Json::str)),
            (
                "width",
                Json::opt(video.size, |size| Json::Float(size.width)),
            ),
            (
                "height",
                Json::opt(video.size, |size| Json::Float(size.height)),
            ),
            (
                "frames",
                Json::opt(video.frames, |frames| {
                    Json::Int(i64::try_from(frames).unwrap_or(i64::MAX))
                }),
            ),
            (
                "audio_channels",
                Json::opt(video.audio_channels, |channels| {
                    Json::Int(i64::from(channels))
                }),
            ),
            (
                "file_size_bytes",
                Json::opt(video.file_size_bytes, |bytes| {
                    Json::Int(i64::try_from(bytes).unwrap_or(i64::MAX))
                }),
            ),
            ("codec", Json::opt(video.codec.as_deref(), Json::str)),
        ])
    });
    let image = match &record.image {
        ImageState::Present { byte_len, .. } => Json::obj([
            ("state", Json::str("present")),
            (
                "bytes",
                Json::Int(i64::try_from(*byte_len).unwrap_or(i64::MAX)),
            ),
        ]),
        ImageState::Evicted { at, .. } => Json::obj([
            ("state", Json::str("evicted")),
            ("evicted_at", Json::Int(at.as_millis())),
        ]),
        ImageState::Absent => Json::obj([("state", Json::str("absent"))]),
    };
    Json::obj([
        ("id", Json::str(record.id.0.as_str())),
        ("created_at", Json::Int(record.created_at.as_millis())),
        ("kind", Json::str(record.media_kind.as_token())),
        ("pinned", Json::Bool(record.pinned)),
        (
            "application",
            Json::opt(record.app_name.as_deref(), Json::str),
        ),
        (
            "title",
            Json::opt(record.window_title.as_deref(), Json::str),
        ),
        ("target", capture_target_json(&record.target)),
        ("image", image),
        ("video", Json::opt(video, |video| video)),
    ])
}

fn history_record_human(record: &CaptureRecord) -> String {
    let pinned = if record.pinned { " pinned" } else { "" };
    let detail = record.video.as_ref().map_or_else(
        || {
            record
                .window_title
                .as_deref()
                .or(record.app_name.as_deref())
                .unwrap_or("screenshot")
                .to_owned()
        },
        |video| match &video.completion {
            VideoCompletion::Complete => {
                format!("{:.2}s {}", video.duration_secs, video.path.display())
            }
            VideoCompletion::Partial {
                salvageability,
                reason,
            } => format!(
                "{:.2}s partial ({}) {} — {reason}",
                video.duration_secs,
                match salvageability {
                    VideoSalvageability::InitialisationOnly => "initialisation only",
                    VideoSalvageability::Playable => "playable",
                },
                video.path.display()
            ),
        },
    );
    format!(
        "{}  {}{pinned}  {detail}",
        record.id.0,
        record.media_kind.as_token()
    )
}

fn history_media_bytes(
    store: &mut scrozz_store::SqliteStore,
    record: &CaptureRecord,
) -> CliResult<Vec<u8>> {
    if let Some(video) = &record.video {
        return std::fs::read(&video.path).map_err(|error| {
            CliError::Core(CoreError::Storage(format!(
                "history media {} is unavailable: {error}",
                video.path.display()
            )))
        });
    }
    let header = record.frame.as_ref().ok_or_else(|| {
        CliError::Core(CoreError::Storage(format!(
            "history item {} has no frame metadata",
            record.id.0
        )))
    })?;
    let data = store.image(&record.id)?.ok_or_else(|| {
        CliError::Core(CoreError::Storage(format!(
            "history item {} no longer has source pixels",
            record.id.0
        )))
    })?;
    FrameEncoder::new()
        .encode(
            &Frame {
                data,
                size: header.size,
                stride: header.stride,
                format: header.format,
                color_space: header.color_space,
                scale: header.scale,
            },
            ImageFormat::Png,
        )
        .map_err(CliError::Core)
}

fn write_new_file(path: &Path, bytes: &[u8]) -> CliResult<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| CliError::Core(CoreError::Io(error)))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| CliError::Core(CoreError::Io(error)))
}

fn copy_new_file(source: &Path, destination: &Path) -> CliResult<()> {
    use std::io::Write as _;

    let mut source =
        std::fs::File::open(source).map_err(|error| CliError::Core(CoreError::Io(error)))?;
    let mut destination = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| CliError::Core(CoreError::Io(error)))?;
    std::io::copy(&mut source, &mut destination)
        .and_then(|_| destination.flush())
        .and_then(|()| destination.sync_all())
        .map(|_| ())
        .map_err(|error| CliError::Core(CoreError::Io(error)))
}

// ---------------------------------------------------------------------------
// ocr
// ---------------------------------------------------------------------------

fn ocr(args: &crate::cli::OcrArgs) -> CliResult<Report> {
    args.validate()?;
    let subject = args.resolve()?;

    // Check the platform before the subject: on Linux there is no engine at all,
    // and reading a file first only to say so afterwards wastes the user's time
    // and makes the failure look conditional when it is not.
    if !platform::ocr_available() {
        return Err(CliError::Core(CoreError::Unsupported {
            what: "recognising text".to_string(),
            why: "this build has no OCR engine. macOS uses Vision, packaged Windows \
                  uses Windows.Media.Ocr, and portable Windows uses its artifact-local \
                  Tesseract payload; Linux has no configured recogniser."
                .to_string(),
        }));
    }

    match subject {
        OcrSubject::File(path) => {
            if !path.exists() {
                return Err(CliError::Core(CoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{} does not exist", path.display()),
                ))));
            }
            let frame = platform::decode_image_file(&path)?;
            let engine = platform::ocr_engine();
            let engine_name = scrozz_ocr::SystemOcr::engine_name()?;
            let blocks = engine.recognize(&frame)?;
            Ok(ocr_report(
                &blocks,
                &path.display().to_string(),
                engine_name,
            ))
        }
        OcrSubject::Capture(_) => {
            let _store = platform::store()?;
            Err(CliError::not_implemented(
                "recognising text in a stored capture",
                "scrozz-store",
            ))
        }
    }
}

/// Renders recognised text for both `--json` and human output.
///
/// The human rendering is **the text and nothing else**, one block per line, so
/// `scrozz ocr shot.png | pbcopy` does the obvious thing. Bounds and confidence
/// belong in `--json`, where a consumer asked for structure; printing them in
/// the human path would corrupt the far more common case of piping the text
/// somewhere.
fn ocr_report(blocks: &[scrozz_ocr::TextBlock], source: &str, engine: &str) -> Report {
    let text = blocks
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let data = Json::obj([
        ("source", Json::str(source)),
        ("engine", Json::str(engine)),
        ("block_count", Json::Int(blocks.len() as i64)),
        ("text", Json::str(text.as_str())),
        (
            "blocks",
            Json::arr(blocks.iter().map(|b| {
                Json::obj([
                    ("text", Json::str(b.text.as_str())),
                    ("confidence", Json::Float(f64::from(b.confidence))),
                    ("x", Json::Float(b.bounds.origin.x)),
                    ("y", Json::Float(b.bounds.origin.y)),
                    ("width", Json::Float(b.bounds.size.width)),
                    ("height", Json::Float(b.bounds.size.height)),
                ])
            })),
        ),
    ]);

    Report::new(data, text)
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

fn settings_command(command: &SettingsCommand) -> CliResult<Report> {
    match command {
        SettingsCommand::Get { key: None } => Ok(Report::new(
            Json::obj([("settings", settings::resolved_all_json()?)]),
            settings::resolved_all_human()?,
        )),

        SettingsCommand::Get { key: Some(key) } => {
            let setting = settings::lookup(key)?;
            Ok(Report::new(
                settings::resolved_json(*setting)?,
                settings::value(setting)?,
            ))
        }

        SettingsCommand::Set { key, value } => {
            // Validate first and completely. A rejected value must be rejected
            // for the right reason: "that is not a format" is useful, "settings
            // are not implemented" is not, and the user needs to hear the first
            // one even while the second is true.
            let setting = settings::lookup(key)?;
            setting.validate(value)?;
            if settings::persist(setting, value)? {
                return Ok(Report::new(
                    settings::resolved_json(*setting)?,
                    format!("{key}={value}"),
                ));
            }
            Err(CliError::not_implemented(
                format!("saving {key}"),
                "scrozz-store (settings persistence)",
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// hotkey
// ---------------------------------------------------------------------------

fn hotkey(command: &HotkeyCommand) -> CliResult<Report> {
    let HotkeyCommand::GenerateConfig {
        compositor,
        action,
        accelerator,
        exec,
    } = command;

    let compositor = resolve_compositor(*compositor)?;
    let config = hotkey_config::generate(compositor, exec, *action, accelerator.as_deref())?;

    Ok(Report::new(config.to_json(), config.to_text()))
}

/// Picks the compositor to target.
///
/// # Errors
///
/// On a system with a working global-shortcut API this is
/// [`CoreError::Unsupported`] rather than a usage error: the command is not
/// misused, it is inapplicable, and saying so explains why the user never needed
/// it.
fn resolve_compositor(explicit: Option<Compositor>) -> CliResult<Compositor> {
    if let Some(compositor) = explicit {
        return Ok(compositor);
    }
    if let Some(detected) = hotkey_config::detect_compositor() {
        return Ok(detected);
    }
    if cfg!(target_os = "linux") {
        return Err(CliError::usage(
            "no sway or Hyprland session was detected; \
             pass --compositor sway or --compositor hyprland",
        ));
    }
    Err(CliError::Core(CoreError::Unsupported {
        what: "generating compositor keybindings".to_string(),
        why: "this command exists for wlroots compositors, which have no \
              global-shortcut portal. This system registers hotkeys directly, so \
              Scrozz sets them up itself and there is nothing to paste. Pass \
              --compositor to generate a fragment anyway."
            .to_string(),
    }))
}

// ---------------------------------------------------------------------------
// gui
// ---------------------------------------------------------------------------

fn gui() -> CliResult<Report> {
    // D27: the GUI has no window at rest, so the first thing it must do is drop
    // out of the Dock. Called here rather than in the launcher so that the
    // ordering is visible: policy first, then anything that could put pixels on
    // screen.
    platform::become_accessory_app()?;

    Err(CliError::not_implemented(
        "launching the menu-bar app",
        "scrozz-ui (the shell has no event loop yet)",
    ))
}

// ---------------------------------------------------------------------------
// shared rendering
// ---------------------------------------------------------------------------

/// The JSON form of a resolved target.
///
/// A tagged object rather than a bare string: `{"kind":"region","x":0,...}` can
/// grow a field without breaking a consumer, and a consumer can switch on `kind`
/// without parsing prose.
#[must_use]
pub fn target_json(target: &TargetSpec) -> Json {
    match target {
        TargetSpec::Region(rect) => Json::obj([
            ("kind", Json::str("region")),
            ("x", Json::Float(rect.origin.x)),
            ("y", Json::Float(rect.origin.y)),
            ("width", Json::Float(rect.size.width)),
            ("height", Json::Float(rect.size.height)),
        ]),
        TargetSpec::Window(selector) => Json::obj([
            ("kind", Json::str("window")),
            ("selector", Json::str(selector.as_str())),
        ]),
        TargetSpec::Display(selector) => Json::obj([
            ("kind", Json::str("display")),
            (
                "selector",
                Json::str(match selector {
                    DisplaySelector::Primary => "primary",
                    DisplaySelector::Active => "active",
                    DisplaySelector::Id(id) => id.as_str(),
                }),
            ),
        ]),
        TargetSpec::AllDisplays => Json::obj([("kind", Json::str("all-displays"))]),
        TargetSpec::Interactive(mode) => Json::obj([
            ("kind", Json::str("interactive")),
            ("mode", Json::str(interactive_slug(*mode))),
        ]),
    }
}

/// The stable slug for an interactive mode.
#[must_use]
pub const fn interactive_slug(mode: InteractiveMode) -> &'static str {
    match mode {
        InteractiveMode::Region => "region",
        InteractiveMode::Window => "window",
        InteractiveMode::Display => "display",
        InteractiveMode::AllInOne => "all-in-one",
    }
}

fn selection_json(options: &SelectionOptions, retake: bool) -> Json {
    Json::obj([
        ("initial_mode", Json::str(options.mode.slug())),
        ("hud", Json::Bool(options.hud)),
        (
            "fixed_size",
            Json::opt(options.constraint.exact, |size| {
                Json::obj([
                    ("width", Json::Float(size.width)),
                    ("height", Json::Float(size.height)),
                ])
            }),
        ),
        (
            "aspect",
            Json::opt(options.constraint.aspect.value(), Json::Float),
        ),
        ("freeze", Json::Bool(options.freeze)),
        ("retake", Json::Bool(retake)),
        ("magnifier", Json::Bool(options.magnifier)),
        ("crosshair", Json::Bool(options.crosshair)),
    ])
}

fn sink_json(sink: &Sink) -> Json {
    match sink {
        Sink::File(path) => Json::obj([
            ("kind", Json::str("file")),
            ("path", path_json(path.as_path())),
        ]),
        other => Json::obj([("kind", Json::str(other.slug()))]),
    }
}

fn path_json(path: &Path) -> Json {
    Json::str(path.to_string_lossy().into_owned())
}

fn describe_target(target: &TargetSpec) -> String {
    match target {
        TargetSpec::Region(rect) => format!(
            "the region {}\u{d7}{} at ({}, {})",
            rect.size.width, rect.size.height, rect.origin.x, rect.origin.y
        ),
        TargetSpec::Window(selector) => format!("the window {selector:?}"),
        TargetSpec::Display(DisplaySelector::Primary) => "the primary display".to_string(),
        TargetSpec::Display(DisplaySelector::Active) => "the active display".to_string(),
        TargetSpec::Display(DisplaySelector::Id(id)) => format!("display {id}"),
        TargetSpec::AllDisplays => "every display".to_string(),
        TargetSpec::Interactive(mode) => {
            format!("an interactively chosen {}", interactive_slug(*mode))
        }
    }
}

fn describe_plan(verb: &str, target: &TargetSpec, sinks: &[Sink]) -> String {
    let destinations: Vec<String> = sinks
        .iter()
        .map(|sink| match sink {
            Sink::File(path) => path.display().to_string(),
            Sink::DefaultFolder => "the capture folder".to_string(),
            Sink::Clipboard => "the clipboard".to_string(),
            Sink::Stdout => "stdout".to_string(),
        })
        .collect();
    format!(
        "{verb} {} to {}.",
        describe_target(target),
        destinations.join(" and ")
    )
}

/// Whether the running instance should handle this command instead.
///
/// Kept next to the handlers so the two are read together: a command that grows
/// shared state must also grow a forwarding policy.
#[must_use]
pub fn should_forward(command: &Command, no_ipc: bool) -> ipc::Forwarding {
    if no_ipc {
        return ipc::Forwarding::Never;
    }
    ipc::policy(command)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use clap::Parser;

    use super::*;
    use crate::{cli::Cli, exit::Exit};

    struct FixedSelector {
        outcome: scrozz_core::SelectionOutcome,
        finished: AtomicBool,
    }

    impl scrozz_core::RegionSelector for FixedSelector {
        fn name(&self) -> &'static str {
            "fixed-recording-selector"
        }

        fn capabilities(&self) -> scrozz_core::SelectionCapabilities {
            scrozz_core::SelectionCapabilities::CLIENT_OVERLAY
        }

        fn select(
            &self,
            _options: &SelectionOptions,
        ) -> scrozz_core::Result<scrozz_core::SelectionOutcome> {
            Ok(self.outcome.clone())
        }
    }

    impl CaptureSelector for FixedSelector {
        fn capture_finished(&self) {
            self.finished.store(true, Ordering::Release);
        }
    }

    fn run(argv: &[&str]) -> CliResult<Report> {
        let cli = Cli::try_parse_from(argv).expect("should parse");
        cli.validate()?;
        dispatch(&cli.command.clone().expect("should have a command"))
    }

    #[test]
    fn forwarded_execution_rejects_expiry_before_dispatch() {
        let execution = ExecutionControl::forwarded(Some(Instant::now()));
        let error = execution.check().unwrap_err();
        assert_eq!(error.exit(), Exit::IpcFailed);
        assert!(error.to_string().contains("expired"));
    }

    #[test]
    fn cancelling_a_forwarded_delay_wakes_it_promptly() {
        let execution = ExecutionControl::forwarded(Some(Instant::now() + Duration::from_secs(5)));
        let canceller = execution.clone();
        let started = Instant::now();
        let worker = std::thread::spawn(move || execution.wait(Duration::from_secs(4)));
        std::thread::sleep(Duration::from_millis(30));
        canceller.cancel();
        let error = worker.join().unwrap().unwrap_err();
        assert_eq!(error.exit(), Exit::IpcFailed);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    fn json_of(argv: &[&str]) -> String {
        run(argv).expect("should succeed").data.to_compact_string()
    }

    struct StoppedSession;

    impl RecordingSession for StoppedSession {
        fn state(&self) -> RecordingState {
            RecordingState::Stopped
        }

        fn pause(&mut self) -> scrozz_core::Result<()> {
            unreachable!("the stopped-session fixture is never paused")
        }

        fn resume(&mut self) -> scrozz_core::Result<()> {
            unreachable!("the stopped-session fixture is never resumed")
        }

        fn stop(self: Box<Self>) -> scrozz_core::Result<Recording> {
            Err(CoreError::Platform("native worker stopped".to_owned()))
        }
    }

    struct FailedPollSession {
        emitted: bool,
    }

    impl RecordingSession for FailedPollSession {
        fn state(&self) -> RecordingState {
            RecordingState::Recording
        }

        fn pause(&mut self) -> scrozz_core::Result<()> {
            unreachable!("the failed-poll fixture is never paused")
        }

        fn resume(&mut self) -> scrozz_core::Result<()> {
            unreachable!("the failed-poll fixture is never resumed")
        }

        fn poll(&mut self) -> Option<SessionEvent> {
            if self.emitted {
                None
            } else {
                self.emitted = true;
                Some(SessionEvent::Failed(Arc::new(CoreError::TargetGone(
                    "window 41".to_owned(),
                ))))
            }
        }

        fn stop(self: Box<Self>) -> scrozz_core::Result<Recording> {
            unreachable!("a terminal poll event logically consumes the session")
        }
    }

    // -- dry-run capture ---------------------------------------------------

    #[test]
    fn a_dry_run_capture_reports_its_plan_without_capturing() {
        let rendered = json_of(&[
            "scrozz",
            "capture",
            "--region",
            "10,20,300,400",
            "--dry-run",
        ]);
        assert!(rendered.contains(r#""dry_run":true"#), "{rendered}");
        assert!(rendered.contains(r#""kind":"region""#), "{rendered}");
        assert!(rendered.contains(r#""x":10.0"#), "{rendered}");
        assert!(rendered.contains(r#""width":300.0"#), "{rendered}");
    }

    #[test]
    fn a_bare_dry_run_capture_defaults_to_an_interactive_region() {
        let rendered = json_of(&["scrozz", "capture", "--dry-run"]);
        assert!(rendered.contains(r#""kind":"interactive""#), "{rendered}");
        assert!(rendered.contains(r#""mode":"region""#), "{rendered}");
        assert!(rendered.contains(r#""interactive":true"#), "{rendered}");
    }

    #[test]
    fn destinations_are_additive_and_ordered() {
        let rendered = json_of(&[
            "scrozz",
            "capture",
            "--all-displays",
            "-o",
            "/x/shot.png",
            "--clipboard",
            "--dry-run",
        ]);
        assert!(
            rendered.contains(r#""kind":"file","path":"/x/shot.png""#),
            "{rendered}"
        );
        assert!(rendered.contains(r#"{"kind":"clipboard"}"#), "{rendered}");
        assert!(!rendered.contains("default-folder"), "{rendered}");
    }

    #[test]
    fn with_no_destination_the_capture_folder_is_used() {
        let rendered = json_of(&["scrozz", "capture", "--dry-run"]);
        assert!(
            rendered.contains(r#"{"kind":"default-folder"}"#),
            "{rendered}"
        );
    }

    #[test]
    fn the_human_plan_reads_as_a_sentence() {
        let report = run(&["scrozz", "capture", "--all-displays", "--dry-run"]).unwrap();
        assert_eq!(
            report.human,
            "Would capture every display to the capture folder."
        );
    }

    #[test]
    fn format_and_quality_reach_the_plan() {
        let rendered = json_of(&[
            "scrozz",
            "capture",
            "--dry-run",
            "--format",
            "webp",
            "--quality",
            "72",
        ]);
        assert!(rendered.contains(r#""format":"webp""#), "{rendered}");
        assert!(rendered.contains(r#""quality":72"#), "{rendered}");
    }

    #[test]
    fn an_unset_quality_is_null_rather_than_missing() {
        let rendered = json_of(&["scrozz", "capture", "--dry-run"]);
        assert!(rendered.contains(r#""quality":null"#), "{rendered}");
        assert!(rendered.contains(r#""delay_secs":null"#), "{rendered}");
    }

    #[test]
    fn window_shadow_is_reported_positively() {
        // The flag is `--no-window-shadow`; the plan says `window_shadow`. A
        // consumer should not have to reason about a double negative.
        let rendered = json_of(&["scrozz", "capture", "--dry-run"]);
        assert!(rendered.contains(r#""window_shadow":true"#), "{rendered}");

        let rendered = json_of(&["scrozz", "capture", "--dry-run", "--no-window-shadow"]);
        assert!(rendered.contains(r#""window_shadow":false"#), "{rendered}");
    }

    #[test]
    fn a_dry_run_never_reaches_a_backend() {
        // The real protection against a stray capture during a test run.
        for argv in [
            vec!["scrozz", "capture", "--dry-run"],
            vec!["scrozz", "capture", "--display", "primary", "--dry-run"],
            vec!["scrozz", "capture", "--window", "Safari", "--dry-run"],
            vec!["scrozz", "record", "--dry-run"],
        ] {
            assert!(run(&argv).is_ok(), "{argv:?}");
        }
    }

    #[test]
    fn a_bad_region_is_rejected_before_anything_else_happens() {
        let cli = Cli::try_parse_from(["scrozz", "capture", "--region", "0,0,0,100", "--dry-run"]);
        assert!(cli.is_err(), "a zero-width region should not parse");
    }

    #[test]
    fn a_negative_delay_is_a_usage_error() {
        let err = run(&["scrozz", "capture", "--delay", "-1", "--dry-run"]).unwrap_err();
        assert_eq!(err.exit(), Exit::Usage);
    }

    #[test]
    fn a_forwarded_delay_cannot_outlive_its_deadline() {
        let execution =
            ExecutionControl::forwarded(Some(Instant::now() + Duration::from_millis(50)));
        let started = Instant::now();
        let error = execution.wait(Duration::from_secs(1)).unwrap_err();
        assert_eq!(error.exit(), Exit::IpcFailed);
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    // -- dry-run record ----------------------------------------------------

    #[test]
    fn a_dry_run_record_reports_its_plan() {
        let rendered = json_of(&[
            "scrozz",
            "record",
            "--dry-run",
            "--fps",
            "60",
            "--microphone",
        ]);
        assert!(rendered.contains(r#""fps":60"#), "{rendered}");
        assert!(rendered.contains(r#""microphone":true"#), "{rendered}");
        assert!(rendered.contains(r#""system_audio":false"#), "{rendered}");
    }

    #[test]
    fn interactive_recording_uses_a_real_selector_and_releases_its_surface() {
        let target = CaptureTarget::Window(scrozz_core::WindowId("selected-window".to_owned()));
        let selector = FixedSelector {
            outcome: scrozz_core::SelectionOutcome {
                mode: scrozz_core::SelectionMode::Window,
                target: target.clone(),
                rect: None,
                display: None,
                scale: scrozz_core::ScaleFactor::IDENTITY,
                source: scrozz_core::SelectionSource::ClientOverlay,
            },
            finished: AtomicBool::new(false),
        };

        assert_eq!(
            select_recording_target(InteractiveMode::Window, Some(&selector)).unwrap(),
            target
        );
        assert!(selector.finished.load(Ordering::Acquire));
    }

    #[test]
    fn a_selected_recording_target_keeps_every_requested_encoding_option() {
        let output = std::env::temp_dir().join("scrozz-selected-recording.mp4");
        let output_arg = output.to_string_lossy().into_owned();
        let cli = Cli::try_parse_from([
            "scrozz",
            "record",
            "--interactive",
            "window",
            "--fps",
            "60",
            "--system-audio",
            "--output",
            &output_arg,
        ])
        .unwrap();
        let Some(Command::Record(args)) = cli.command else {
            panic!("expected record command");
        };
        let target = CaptureTarget::Window(scrozz_core::WindowId("selected-window".to_owned()));

        let prepared = prepare_recording_args_for_target(&args, target.clone()).unwrap();

        assert_eq!(prepared.request.target, target);
        assert_eq!(prepared.request.fps, 60);
        assert!(prepared.request.system_audio);
        assert_eq!(
            prepared.request.destination.as_deref(),
            Some(output.as_path())
        );
    }

    #[test]
    fn stopping_with_nothing_running_explains_itself() {
        let err = run(&["scrozz", "record", "--stop"]).unwrap_err();
        assert_eq!(err.exit(), Exit::InvalidRequest);
        assert!(
            err.to_string().contains("no recording is in progress"),
            "{err}"
        );
    }

    #[test]
    fn a_stopped_native_session_is_finalised_without_a_terminal_poll_event() {
        let mut manager = RecordingManager::default();
        manager
            .start(
                Box::new(StoppedSession),
                PathBuf::from("stopped.mp4"),
                CaptureTarget::AllDisplays,
                Json::Bool(false),
            )
            .unwrap();

        manager.poll();

        assert!(!manager.is_active());
        let error = manager
            .take_completion()
            .expect("terminal completion")
            .unwrap_err();
        assert!(error.to_string().contains("native worker stopped"));
    }

    #[test]
    fn an_asynchronous_failure_keeps_its_stable_error_classification() {
        let mut manager = RecordingManager::default();
        manager
            .start(
                Box::new(FailedPollSession { emitted: false }),
                PathBuf::from("failed.mp4"),
                CaptureTarget::Window(scrozz_core::WindowId("41".into())),
                Json::Bool(false),
            )
            .unwrap();

        manager.poll();

        let error = manager
            .take_completion()
            .expect("terminal completion")
            .unwrap_err();
        assert_eq!(error.exit(), Exit::TargetGone);
        assert_eq!(error.kind(), "target-gone");
        assert!(error.to_json().to_compact_string().contains("window 41"));
    }

    #[test]
    fn a_queued_start_cannot_replace_an_undelivered_terminal_outcome() {
        let mut manager = RecordingManager::default();
        manager
            .start(
                Box::new(StoppedSession),
                PathBuf::from("first.mp4"),
                CaptureTarget::AllDisplays,
                Json::Bool(false),
            )
            .unwrap();
        manager.poll();

        let error = manager
            .start(
                Box::new(StoppedSession),
                PathBuf::from("second.mp4"),
                CaptureTarget::AllDisplays,
                Json::Bool(false),
            )
            .expect_err("pending terminal outcome owns the manager");
        assert!(error.to_string().contains("has not been delivered"));

        let original = manager
            .take_completion()
            .expect("original terminal outcome")
            .unwrap_err();
        assert!(original.to_string().contains("native worker stopped"));
    }

    // -- unimplemented paths -----------------------------------------------

    #[test]
    fn every_unimplemented_command_reports_rather_than_panics() {
        let cases = [
            vec!["scrozz", "capture", "--region", "0,0,10,10"],
            vec!["scrozz", "gui"],
        ];
        for argv in cases {
            let err = run(&argv).unwrap_err();
            assert!(
                !err.is_cancellation(),
                "{argv:?} should not look like a cancellation"
            );
            assert_ne!(err.exit(), Exit::Success, "{argv:?}");
        }
    }

    #[test]
    fn malformed_history_ids_are_rejected_before_touching_the_store() {
        let error = history_id("../not-an-id").expect_err("paths are not capture ids");
        assert_eq!(error.exit(), Exit::Usage);
    }

    #[test]
    fn launching_the_gui_from_a_test_does_nothing_visible() {
        // Load-bearing: the test suite must never put a window on screen.
        let err = run(&["scrozz", "gui"]).unwrap_err();
        assert_eq!(err.exit(), Exit::NotImplemented);
    }

    // -- settings ----------------------------------------------------------

    #[test]
    fn listing_settings_works_today() {
        let rendered = json_of(&["scrozz", "settings", "get"]);
        assert!(rendered.contains(r#""key":"capture.format""#), "{rendered}");
        assert!(
            rendered.contains(r#""key":"hotkey.record-stop""#),
            "{rendered}"
        );
    }

    #[test]
    fn reading_one_setting_returns_just_that_one() {
        let report = run(&["scrozz", "settings", "get", "capture.quality"]).unwrap();
        assert_eq!(report.human, "90");
        assert!(
            report
                .data
                .to_compact_string()
                .contains(r#""key":"capture.quality""#)
        );
    }

    #[test]
    fn an_unknown_setting_is_a_usage_error_with_a_suggestion() {
        let err = run(&["scrozz", "settings", "get", "capture.forrmat"]).unwrap_err();
        assert_eq!(err.exit(), Exit::Usage);
        assert!(err.to_string().contains("capture.format"), "{err}");
    }

    #[test]
    fn a_bad_value_is_rejected_for_being_bad_not_for_being_unimplemented() {
        // The distinction that matters: the user must learn about their mistake
        // even though persistence is missing.
        let err = run(&["scrozz", "settings", "set", "capture.format", "gif"]).unwrap_err();
        assert_eq!(err.exit(), Exit::Usage);
        assert!(err.to_string().contains("png"), "{err}");
    }

    #[test]
    fn a_good_value_reports_the_missing_persistence() {
        let err = run(&["scrozz", "settings", "set", "capture.format", "webp"]).unwrap_err();
        assert_eq!(err.exit(), Exit::NotImplemented);
        assert!(err.to_string().contains("capture.format"), "{err}");
    }

    // -- hotkey ------------------------------------------------------------

    #[test]
    fn generating_a_sway_config_works_today() {
        let report = run(&[
            "scrozz",
            "hotkey",
            "generate-config",
            "--compositor",
            "sway",
        ])
        .unwrap();
        assert!(
            report
                .human
                .contains("bindsym Mod4+Shift+4 exec scrozz capture")
        );
        assert!(
            report
                .data
                .to_compact_string()
                .contains(r#""compositor":"sway""#)
        );
    }

    #[test]
    fn generating_a_hyprland_config_works_today() {
        let report = run(&[
            "scrozz",
            "hotkey",
            "generate-config",
            "--compositor",
            "hyprland",
        ])
        .unwrap();
        assert!(
            report
                .human
                .contains("bind = SUPER SHIFT, 4, exec, scrozz capture")
        );
    }

    #[test]
    fn a_single_action_can_be_generated() {
        let report = run(&[
            "scrozz",
            "hotkey",
            "generate-config",
            "--compositor",
            "sway",
            "--action",
            "record-stop",
        ])
        .unwrap();
        assert_eq!(
            report
                .human
                .lines()
                .filter(|l| l.starts_with("bindsym"))
                .count(),
            1
        );
        assert!(report.human.contains("record --stop"));
    }

    #[test]
    fn a_custom_exec_path_is_honoured() {
        let report = run(&[
            "scrozz",
            "hotkey",
            "generate-config",
            "--compositor",
            "sway",
            "--exec",
            "/usr/local/bin/scrozz",
        ])
        .unwrap();
        assert!(report.human.contains("exec /usr/local/bin/scrozz capture"));
    }

    #[test]
    fn no_compositor_and_no_flag_explains_rather_than_fails_obscurely() {
        let _env = crate::test_env::lock();
        let err = run(&["scrozz", "hotkey", "generate-config"]);
        match err {
            // On a wlroots session the command legitimately succeeds.
            Ok(_) => assert!(hotkey_config::detect_compositor().is_some()),
            Err(e) => {
                let exit = e.exit();
                assert!(
                    matches!(exit, Exit::Usage | Exit::Unsupported),
                    "unexpected exit {exit:?}"
                );
                assert!(e.to_string().contains("--compositor"), "{e}");
            }
        }
    }

    // -- list --------------------------------------------------------------

    #[test]
    fn listing_windows_under_wayland_explains_the_protocol_gap() {
        let _env = crate::test_env::lock();
        crate::test_env::set("WAYLAND_DISPLAY", "wayland-0");
        let err = run(&["scrozz", "list", "windows"]).unwrap_err();

        assert_eq!(err.exit(), Exit::Unsupported);
        let text = err.to_human();
        assert!(text.contains("no window enumeration protocol"), "{text}");
        // D8 requires a real alternative, not a route that would need to invent
        // a window id for the portal's opaque choice.
        assert!(text.contains("Capture a display instead"), "{text}");
        assert!(!text.contains("--interactive window"), "{text}");
    }

    #[test]
    fn listing_displays_is_never_an_unsupported_platform_error() {
        // Every platform can enumerate displays; only windows are contentious.
        let err = list(ListWhat::Displays).unwrap_err();
        assert_eq!(err.exit(), Exit::NotImplemented);
    }

    // -- ocr ---------------------------------------------------------------

    #[test]
    fn ocr_on_a_platform_without_an_engine_says_why() {
        if platform::ocr_available() {
            return;
        }
        let err = run(&["scrozz", "ocr", "--capture", "abc"]).unwrap_err();
        assert_eq!(err.exit(), Exit::Unsupported);
        assert!(err.to_string().contains("no system recogniser"), "{err}");
    }

    #[test]
    fn ocr_on_a_missing_file_reports_the_file_not_the_backend() {
        if !platform::ocr_available() {
            return;
        }
        let err = run(&["scrozz", "ocr", "--file", "./definitely-not-here.png"]).unwrap_err();
        assert_eq!(err.exit(), Exit::Io);
    }

    #[test]
    fn ocr_json_contract_includes_the_stable_engine_key() {
        let report = ocr_report(&[], "fixture.png", "windows-media-ocr");
        assert_eq!(
            report.data.to_compact_string(),
            r#"{"source":"fixture.png","engine":"windows-media-ocr","block_count":0,"text":"","blocks":[]}"#
        );
    }

    // -- shared rendering --------------------------------------------------

    #[test]
    fn every_target_kind_renders_with_a_tag() {
        let cases = [
            (
                vec!["scrozz", "capture", "--region", "1,2,3,4", "--dry-run"],
                "region",
            ),
            (
                vec!["scrozz", "capture", "--window", "Safari", "--dry-run"],
                "window",
            ),
            (
                vec!["scrozz", "capture", "--display", "primary", "--dry-run"],
                "display",
            ),
            (
                vec!["scrozz", "capture", "--all-displays", "--dry-run"],
                "all-displays",
            ),
            (
                vec!["scrozz", "capture", "--interactive", "--dry-run"],
                "interactive",
            ),
        ];
        for (argv, kind) in cases {
            let rendered = json_of(&argv);
            assert!(
                rendered.contains(&format!(r#""kind":"{kind}""#)),
                "{argv:?} produced {rendered}"
            );
        }
    }

    #[test]
    fn interactive_slugs_match_the_value_enum_spelling() {
        assert_eq!(interactive_slug(InteractiveMode::Region), "region");
        assert_eq!(interactive_slug(InteractiveMode::Window), "window");
        assert_eq!(interactive_slug(InteractiveMode::Display), "display");
    }

    #[test]
    fn no_ipc_overrides_every_forwarding_policy() {
        let cli = Cli::try_parse_from(["scrozz", "record", "--stop"]).unwrap();
        let command = cli.command.unwrap();
        assert_eq!(should_forward(&command, false), ipc::Forwarding::Require);
        assert_eq!(should_forward(&command, true), ipc::Forwarding::Never);
    }
}
