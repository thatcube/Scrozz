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

use std::{
    io::IsTerminal,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU8, Ordering},
    },
};

use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, CursorMode, Display,
    Error as CoreError, Frame, LogicalPoint, LogicalRect, LogicalSize, PhysicalSize, ScrollAxis,
    ScrollGesture, Window, WindowId,
};
use scrozz_export::{Clipboard, Encoder, FrameEncoder};
use scrozz_ocr::Ocr as _;
use scrozz_stitch::{
    CancelAction, CancelSignal, FrameSource, Progress, ScrollSession, ScrollSessionConfig,
    ThreadPacer,
};

use crate::{
    cli::{
        CaptureArgs, Command, Compositor, DisplaySelector, HistoryCommand, HotkeyCommand,
        InteractiveMode, ListWhat, OcrSubject, RecordArgs, SettingsCommand, Sink, TargetSpec,
    },
    fault::{CliError, CliResult},
    hotkey_config, ipc,
    json::Json,
    platform,
    report::Report,
    settings,
};

const WAYLAND_PORTAL_PICKER_WINDOW_ID: &str = "xdg-desktop-portal-picker";

/// Runs a command locally.
///
/// # Errors
///
/// Whatever the command produces. Cancellation arrives here as
/// [`scrozz_core::Error::Cancelled`] and is rendered as an outcome, not a fault.
pub fn dispatch(command: &Command) -> CliResult<Report> {
    dispatch_inner(command, None)
}

/// Runs a command while allowing an owner to cancel interactive capture.
///
/// Used by the GUI's forwarded-command worker so a portal picker cannot keep
/// application shutdown waiting. The ordinary CLI remains synchronous through
/// [`dispatch`].
pub(crate) fn dispatch_with_cancellation(
    command: &Command,
    cancellation: &scrozz_capture::CaptureCancellation,
) -> CliResult<Report> {
    if cancellation.is_cancelled() {
        return Err(CliError::Core(CoreError::Cancelled));
    }
    dispatch_inner(command, Some(cancellation))
}

fn dispatch_inner(
    command: &Command,
    cancellation: Option<&scrozz_capture::CaptureCancellation>,
) -> CliResult<Report> {
    match command {
        Command::Capture(args) => capture(args, cancellation),
        Command::Record(args) => record(args),
        Command::List(args) => list(args.what),
        Command::History(args) => history(&args.command),
        Command::Ocr(args) => ocr(args),
        Command::Settings(args) => settings_command(&args.command),
        Command::Hotkey(args) => hotkey(&args.command),
        Command::Gui => gui(),
    }
}

// ---------------------------------------------------------------------------
// capture
// ---------------------------------------------------------------------------

fn capture(
    args: &CaptureArgs,
    cancellation: Option<&scrozz_capture::CaptureCancellation>,
) -> CliResult<Report> {
    args.validate()?;
    let target = args.target_spec()?;
    let sinks = args.sinks();

    let plan = Json::obj([
        ("target", target_json(&target)),
        (
            "interactive",
            Json::Bool(matches!(target, TargetSpec::Interactive(_))),
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
            describe_plan("Would capture", &target, &sinks),
        ));
    }

    // The delay is deliberately *not* honoured before the backend check. Making
    // a user wait five seconds to be told the feature is unimplemented is a
    // small cruelty that costs nothing to avoid.
    let backend = platform::capture_backend()?;
    let request = CaptureRequest {
        target: capture_target(&target)?,
        cursor: if args.cursor {
            CursorMode::Visible
        } else {
            CursorMode::Hidden
        },
        include_window_shadow: !args.no_window_shadow,
    };

    wait_for_capture_delay(args.delay, cancellation)?;

    let (capture, mut terminal_cancel) = match target {
        TargetSpec::Scrolling { axis, .. } => {
            let (capture, cancel) =
                scrolling_capture(backend.as_ref(), request, axis, cancellation)?;
            (capture, Some(cancel))
        }
        _ => {
            let capture = match cancellation {
                Some(cancellation) => platform::capture_with_cancellation(&request, cancellation)?,
                None => backend.capture(&request)?,
            };
            (capture, None)
        }
    };
    fail_if_terminal_abort(&mut terminal_cancel)?;
    let frame = &capture.frame;

    let bytes = FrameEncoder::new()
        .encode(frame, args.format().to_export())
        .map_err(CliError::Core)?;
    fail_if_terminal_abort(&mut terminal_cancel)?;

    let mut prepared = Vec::with_capacity(sinks.len());
    for sink in &sinks {
        fail_if_terminal_abort(&mut terminal_cancel)?;
        match sink {
            Sink::File(path) => prepared.push(PreparedSink::File(
                crate::output::StagedFile::for_path(&bytes, path.clone())?,
            )),
            Sink::Clipboard => prepared.push(PreparedSink::Clipboard),
            Sink::Stdout => prepared.push(PreparedSink::Stdout),
            Sink::DefaultFolder => prepared.push(PreparedSink::File(
                crate::output::StagedFile::for_default(&bytes)?,
            )),
        }
    }

    // This is the single irreversible boundary. Abort wins before it; once this
    // compare-exchange wins, the signal handler can no longer turn a published
    // output into a cancellation report.
    seal_terminal_output(&mut terminal_cancel)?;
    let mut written = Vec::new();
    let mut raw = None;
    for sink in prepared {
        match sink {
            PreparedSink::File(staged) => {
                written.push(staged.commit()?.display().to_string());
            }
            PreparedSink::Clipboard => {
                scrozz_export::SystemClipboard::new()
                    .write_image(frame)
                    .map_err(CliError::Core)?;
                written.push("clipboard".to_owned());
            }
            PreparedSink::Stdout => raw = Some(bytes.clone()),
        }
    }

    let data = Json::obj([
        ("plan", plan),
        ("width", Json::Int(i64::from(frame.width()))),
        ("height", Json::Int(i64::from(frame.height()))),
        ("scale", Json::Float(frame.scale.get())),
        ("bytes", Json::Int(bytes.len() as i64)),
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

enum PreparedSink {
    File(crate::output::StagedFile),
    Clipboard,
    Stdout,
}

fn wait_for_capture_delay(
    seconds: Option<f64>,
    cancellation: Option<&scrozz_capture::CaptureCancellation>,
) -> CliResult<()> {
    let Some(seconds) = seconds else {
        return Ok(());
    };
    let mut remaining = std::time::Duration::try_from_secs_f64(seconds)
        .map_err(|_| CliError::usage(format!("--delay is too large: {seconds} seconds")))?;
    if cancellation.is_none() {
        std::thread::sleep(remaining);
        return Ok(());
    }
    const POLL: std::time::Duration = std::time::Duration::from_millis(25);

    while !remaining.is_zero() {
        if cancellation.is_some_and(scrozz_capture::CaptureCancellation::is_cancelled) {
            return Err(CliError::Core(CoreError::Cancelled));
        }
        let step = remaining.min(POLL);
        std::thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }

    if cancellation.is_some_and(scrozz_capture::CaptureCancellation::is_cancelled) {
        Err(CliError::Core(CoreError::Cancelled))
    } else {
        Ok(())
    }
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
        TargetSpec::Scrolling { display, .. } if is_wayland() => {
            wayland_scrolling_capture_target(display)
        }
        TargetSpec::Display(sel) | TargetSpec::Scrolling { display: sel, .. } => {
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

fn scrolling_capture(
    backend: &dyn CaptureBackend,
    request: CaptureRequest,
    axis: ScrollAxis,
    acquisition_cancellation: Option<&scrozz_capture::CaptureCancellation>,
) -> CliResult<(Capture, TerminalCancellation)> {
    let mut cancel = TerminalCancellation::install()?;
    let acquisition_cancellation = acquisition_cancellation
        .cloned()
        .unwrap_or_else(|| cancel.acquisition.clone());
    let target = resolve_scrolling_target(backend, request)?;
    let capture = if std::io::stderr().is_terminal() {
        eprintln!(
            "scrozz: scrolling capture started; press Ctrl+C once to keep the stitched result, \
             or twice quickly to discard it"
        );
        scrolling_capture_target_with_cancellation(
            target,
            axis,
            &mut cancel,
            &acquisition_cancellation,
            report_terminal_scroll_progress,
        )
    } else {
        scrolling_capture_target_with_cancellation(
            target,
            axis,
            &mut cancel,
            &acquisition_cancellation,
            |event| tracing::debug!(?event, "scrolling capture progress"),
        )
    }?;
    Ok((capture, cancel))
}

struct TerminalHandler {
    state: Arc<AtomicU8>,
    acquisition: Arc<Mutex<scrozz_capture::CaptureCancellation>>,
    install_error: Option<String>,
}

static TERMINAL_HANDLER: OnceLock<TerminalHandler> = OnceLock::new();

struct TerminalCancellation {
    state: Arc<AtomicU8>,
    acquisition: scrozz_capture::CaptureCancellation,
}

impl TerminalCancellation {
    fn install() -> CliResult<Self> {
        let handler = TERMINAL_HANDLER.get_or_init(|| {
            let state = Arc::new(AtomicU8::new(0));
            let acquisition = Arc::new(Mutex::new(scrozz_capture::CaptureCancellation::new()));
            let signal_state = Arc::clone(&state);
            let signal_acquisition = Arc::clone(&acquisition);
            let install_error = ctrlc::set_handler(move || {
                let _ = advance_terminal_cancellation(&signal_state);
                signal_acquisition
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .cancel();
            })
            .err()
            .map(|error| error.to_string());
            TerminalHandler {
                state,
                acquisition,
                install_error,
            }
        });
        if let Some(error) = &handler.install_error {
            return Err(CliError::Core(CoreError::Platform(format!(
                "could not install scrolling-capture cancellation: {error}"
            ))));
        }
        let acquisition = scrozz_capture::CaptureCancellation::new();
        *handler
            .acquisition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = acquisition.clone();
        handler.state.store(0, Ordering::Release);
        Ok(Self {
            state: Arc::clone(&handler.state),
            acquisition,
        })
    }
}

impl CancelSignal for TerminalCancellation {
    fn cancellation(&mut self) -> Option<CancelAction> {
        match self.state.load(Ordering::Acquire) {
            1 => Some(CancelAction::Keep),
            2 => Some(CancelAction::Abort),
            _ => None,
        }
    }
}

fn fail_if_terminal_abort(cancel: &mut Option<TerminalCancellation>) -> CliResult<()> {
    if cancel
        .as_mut()
        .and_then(|signal| signal.cancellation())
        .is_some_and(|action| action == CancelAction::Abort)
    {
        return Err(CliError::Core(CoreError::Cancelled));
    }
    Ok(())
}

fn seal_terminal_output(cancel: &mut Option<TerminalCancellation>) -> CliResult<()> {
    let Some(cancel) = cancel else {
        return Ok(());
    };
    loop {
        match cancel.state.load(Ordering::Acquire) {
            2 => return Err(CliError::Core(CoreError::Cancelled)),
            3 => return Ok(()),
            state @ (0 | 1) => {
                if cancel
                    .state
                    .compare_exchange(state, 3, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Ok(());
                }
            }
            _ => return Ok(()),
        }
    }
}

fn advance_terminal_cancellation(state: &AtomicU8) -> Option<u8> {
    loop {
        let current = state.load(Ordering::Acquire);
        let next = match current {
            0 => 1,
            1 => 2,
            // Publication has begun. A late signal must not report cancellation
            // for output that is already becoming visible.
            _ => return None,
        };
        if state
            .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(next);
        }
    }
}

fn report_terminal_scroll_progress(progress: Progress) {
    match progress {
        Progress::Prepared {
            driver,
            automatic: true,
            ..
        } => eprintln!("scrozz: {driver} will move the selected window"),
        Progress::Prepared {
            manual_reason: Some(reason),
            ..
        } => eprintln!("scrozz: scroll the selected window manually; {reason}"),
        Progress::Prepared { .. } => {
            eprintln!("scrozz: scroll the selected window manually while capture follows");
        }
        Progress::FrameCaptured { frame } => {
            eprintln!("scrozz: captured viewport {frame}");
        }
        Progress::WaitingForManualScroll => {
            eprintln!("scrozz: waiting for the selected window to scroll");
        }
        Progress::Advanced {
            frame,
            delta,
            output_extent,
            ..
        } => eprintln!(
            "scrozz: stitched viewport {frame} ({delta} px movement, {output_extent} px total)"
        ),
        Progress::Stalled { count } => {
            eprintln!("scrozz: no movement detected (probe {count})");
        }
        Progress::Interrupted { reason } => {
            eprintln!("scrozz: keeping the valid stitched prefix after: {reason}");
        }
        Progress::Finished { reason, .. } => {
            eprintln!("scrozz: scrolling capture finished ({reason:?})");
        }
    }
}

pub(crate) fn scrolling_capture_with<C, F>(
    backend: &dyn CaptureBackend,
    request: CaptureRequest,
    axis: ScrollAxis,
    cancel: &mut C,
    progress: F,
) -> CliResult<Capture>
where
    C: CancelSignal,
    F: FnMut(Progress),
{
    let target = resolve_scrolling_target(backend, request)?;
    scrolling_capture_target_with(target, axis, cancel, progress)
}

#[derive(Debug, Clone)]
pub(crate) struct ScrollingTarget {
    request: CaptureRequest,
    context: ScrollingContext,
}

#[derive(Debug, Clone)]
enum ScrollingContext {
    Native {
        display: Box<Display>,
        viewport: LogicalRect,
        window: WindowId,
        crop: Option<FrameCrop>,
    },
    ManualPortal,
}

impl ScrollingTarget {
    pub(crate) fn new(
        request: CaptureRequest,
        display: Display,
        viewport: LogicalRect,
        window: WindowId,
    ) -> Self {
        Self {
            request,
            context: ScrollingContext::Native {
                display: Box::new(display),
                viewport,
                window,
                crop: None,
            },
        }
    }

    fn with_crop(mut self, crop: FrameCrop) -> Self {
        if let ScrollingContext::Native {
            crop: target_crop, ..
        } = &mut self.context
        {
            *target_crop = Some(crop);
        }
        self
    }

    fn manual_portal(request: CaptureRequest) -> Self {
        Self {
            request,
            context: ScrollingContext::ManualPortal,
        }
    }

    pub(crate) fn capture_target(&self) -> CaptureTarget {
        self.request.target.clone()
    }

    pub(crate) fn may_synthesize_scroll(&self) -> bool {
        matches!(self.context, ScrollingContext::Native { .. })
    }

    pub(crate) fn refresh(self, backend: &dyn CaptureBackend) -> CliResult<Self> {
        if matches!(&self.context, ScrollingContext::ManualPortal) {
            return Ok(self);
        }
        let windows = backend.windows()?;
        let displays = backend.displays()?;
        self.refresh_from_snapshots(windows, displays)
    }

    fn refresh_from_snapshots(
        self,
        windows: Vec<Window>,
        displays: Vec<Display>,
    ) -> CliResult<Self> {
        let window_id = match &self.context {
            ScrollingContext::ManualPortal => return Ok(self),
            ScrollingContext::Native { window, .. } => window.clone(),
        };
        let window = windows
            .into_iter()
            .find(|window| window.id == window_id)
            .ok_or_else(|| {
                CliError::Core(CoreError::TargetGone(format!(
                    "window {} vanished before scrolling capture started",
                    window_id.0
                )))
            })?;
        if !window.is_visible || window.bounds.is_empty() {
            return Err(CliError::Core(CoreError::TargetGone(format!(
                "window {} is no longer visible for scrolling capture",
                window_id.0
            ))));
        }
        let display = displays
            .into_iter()
            .find(|display| display.id == window.display)
            .ok_or_else(|| {
                CliError::Core(CoreError::TargetGone(format!(
                    "display {} containing window {} is no longer connected",
                    window.display.0, window_id.0
                )))
            })?;

        resolved_native_scrolling_target(self.request, display, window)
    }

    fn session_config(&self, axis: ScrollAxis) -> CliResult<ScrollSessionConfig> {
        match &self.context {
            ScrollingContext::Native {
                display,
                viewport,
                window,
                ..
            } => scroll_session_config(display, *viewport, axis, window.clone()),
            ScrollingContext::ManualPortal => {
                let at = LogicalPoint::new(0.0, 0.0);
                let gesture = match axis {
                    ScrollAxis::Vertical => ScrollGesture::down(at, 1.0),
                    ScrollAxis::Horizontal => ScrollGesture::right(at, 1.0),
                };
                Ok(ScrollSessionConfig::new(gesture))
            }
        }
    }
}

pub(crate) fn resolve_scrolling_target(
    backend: &dyn CaptureBackend,
    request: CaptureRequest,
) -> CliResult<ScrollingTarget> {
    if is_wayland() && request.target == wayland_portal_picker_target() {
        return Ok(ScrollingTarget::manual_portal(request));
    }
    if request.target.is_window() {
        return Err(CliError::Core(CoreError::InvalidRequest(
            "scrolling capture cannot treat an ordinary window id as a Wayland portal choice"
                .to_owned(),
        )));
    }
    let CaptureTarget::Display(display_id) = &request.target else {
        return Err(CliError::Core(CoreError::InvalidRequest(
            "scrolling capture requires one display or a Wayland portal window".to_owned(),
        )));
    };
    let display = backend
        .displays()?
        .into_iter()
        .find(|display| display.id == *display_id)
        .ok_or_else(|| {
            CliError::Core(CoreError::TargetGone(format!(
                "display {} vanished before scrolling capture started",
                display_id.0
            )))
        })?;
    let window = match backend.windows() {
        Ok(windows) => select_scrolling_window(windows, &display),
        Err(error) => return Err(CliError::Core(error)),
    }
    .ok_or_else(|| {
        CliError::Core(CoreError::InvalidRequest(format!(
            "no visible application window is available on display {}; focus the window to \
             capture and retry (or use --delay to switch to it after starting the command)",
            display.id.0
        )))
    })?;
    resolved_native_scrolling_target(request, display, window)
}

pub(crate) fn scrolling_capture_target_with<C, F>(
    target: ScrollingTarget,
    axis: ScrollAxis,
    cancel: &mut C,
    progress: F,
) -> CliResult<Capture>
where
    C: CancelSignal,
    F: FnMut(Progress),
{
    let acquisition_cancellation = scrozz_capture::CaptureCancellation::new();
    scrolling_capture_target_with_cancellation(
        target,
        axis,
        cancel,
        &acquisition_cancellation,
        progress,
    )
}

pub(crate) fn scrolling_capture_target_with_cancellation<C, F>(
    target: ScrollingTarget,
    axis: ScrollAxis,
    cancel: &mut C,
    acquisition_cancellation: &scrozz_capture::CaptureCancellation,
    progress: F,
) -> CliResult<Capture>
where
    C: CancelSignal,
    F: FnMut(Progress),
{
    if acquisition_cancellation.is_cancelled() {
        return Err(CliError::Core(CoreError::Cancelled));
    }
    let config = target.session_config(axis)?;
    let crop = match &target.context {
        ScrollingContext::Native { crop, .. } => *crop,
        ScrollingContext::ManualPortal => None,
    };
    let request = target.request;
    let source = CaptureFrameSource {
        session: scrozz_capture::frame_session_with_cancellation(
            request.clone(),
            acquisition_cancellation,
        )
        .map_err(CliError::Core)?,
        crop,
        acquisition_cancellation: acquisition_cancellation.clone(),
    };
    let driver = platform::scroll_driver()?;
    let output = ScrollSession::new(source, driver, ThreadPacer, config).run(cancel, progress)?;
    Ok(output.into_capture(request.target))
}

struct CaptureFrameSource {
    session: Box<dyn scrozz_capture::FrameSession>,
    crop: Option<FrameCrop>,
    acquisition_cancellation: scrozz_capture::CaptureCancellation,
}

impl FrameSource for CaptureFrameSource {
    fn capture_frame(&mut self) -> scrozz_core::Result<scrozz_core::Frame> {
        if self.acquisition_cancellation.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let frame = self.session.capture_frame()?;
        if self.acquisition_cancellation.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        match self.crop {
            Some(crop) => crop.apply(frame),
            None => Ok(frame),
        }
    }

    fn name(&self) -> &str {
        self.session.name()
    }
}

fn resolved_native_scrolling_target(
    mut request: CaptureRequest,
    display: Display,
    window: Window,
) -> CliResult<ScrollingTarget> {
    let viewport = intersect_rect(window.bounds, display.work_area).ok_or_else(|| {
        CliError::Core(CoreError::InvalidRequest(format!(
            "the selected window is outside display {}'s usable area",
            display.id.0
        )))
    })?;
    let crop = FrameCrop::new(window.bounds, viewport)?;
    let window_id = window.id;
    request.target = CaptureTarget::Window(window_id.clone());
    // X11 enumeration reports the outer frame while a shadowless capture reads
    // only the client drawable. Capture the frame window so work-area clipping
    // and frame pixels share one coordinate space.
    request.include_window_shadow = needs_outer_frame_capture(&window_id);
    Ok(ScrollingTarget::new(request, display, viewport, window_id).with_crop(crop))
}

fn needs_outer_frame_capture(window: &WindowId) -> bool {
    cfg!(target_os = "linux") && window.0.starts_with("x11:")
}

pub(crate) fn wayland_portal_picker_target() -> CaptureTarget {
    CaptureTarget::Window(WindowId(WAYLAND_PORTAL_PICKER_WINDOW_ID.to_owned()))
}

fn wayland_scrolling_capture_target(display: &DisplaySelector) -> CliResult<CaptureTarget> {
    if *display != DisplaySelector::Active {
        return Err(CliError::Core(CoreError::Unsupported {
            what: "Wayland scrolling capture with an explicit display selector".to_owned(),
            why: "Wayland can safely capture one scrolling window only through the desktop \
                  portal picker; omit the selector or use `--scrolling=active`"
                .to_owned(),
        }));
    }
    Ok(wayland_portal_picker_target())
}

#[derive(Debug, Clone, Copy)]
struct FrameCrop {
    source_bounds: LogicalRect,
    viewport: LogicalRect,
}

impl FrameCrop {
    fn new(source_bounds: LogicalRect, viewport: LogicalRect) -> CliResult<Self> {
        if source_bounds.is_empty() || viewport.is_empty() {
            return Err(CliError::Core(CoreError::InvalidRequest(
                "scrolling viewport must have positive dimensions".to_owned(),
            )));
        }
        Ok(Self {
            source_bounds,
            viewport,
        })
    }

    fn apply(self, frame: Frame) -> scrozz_core::Result<Frame> {
        if frame.width() == 0 || frame.height() == 0 || !frame.is_well_formed() {
            return Err(CoreError::Platform(
                "the scrolling frame source returned malformed pixel geometry".to_owned(),
            ));
        }
        let relative_x = self.viewport.origin.x - self.source_bounds.origin.x;
        let relative_y = self.viewport.origin.y - self.source_bounds.origin.y;
        let x_scale = f64::from(frame.width()) / self.source_bounds.size.width;
        let y_scale = f64::from(frame.height()) / self.source_bounds.size.height;

        // Round inward so pixels outside the selected display/work area can
        // never enter matching or output, even at fractional scale factors.
        let left = (relative_x.max(0.0) * x_scale).ceil() as u32;
        let top = (relative_y.max(0.0) * y_scale).ceil() as u32;
        let right = ((relative_x + self.viewport.size.width).min(self.source_bounds.size.width)
            * x_scale)
            .floor() as u32;
        let bottom = ((relative_y + self.viewport.size.height).min(self.source_bounds.size.height)
            * y_scale)
            .floor() as u32;

        if left >= right || top >= bottom || right > frame.width() || bottom > frame.height() {
            return Err(CoreError::Platform(format!(
                "selected scrolling viewport maps outside the captured frame: \
                 crop=({left},{top})..({right},{bottom}), frame={}x{}",
                frame.width(),
                frame.height()
            )));
        }
        if left == 0 && top == 0 && right == frame.width() && bottom == frame.height() {
            return Ok(frame);
        }

        let bytes_per_pixel = frame.format.bytes_per_pixel();
        let width = right - left;
        let height = bottom - top;
        let row_bytes = width as usize * bytes_per_pixel;
        let mut pixels = Vec::with_capacity(row_bytes * height as usize);
        for y in top..bottom {
            let start = y as usize * frame.stride + left as usize * bytes_per_pixel;
            pixels.extend_from_slice(&frame.data[start..start + row_bytes]);
        }

        Ok(Frame {
            data: pixels,
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            stride: row_bytes,
            format: frame.format,
            color_space: frame.color_space,
            scale: frame.scale,
        })
    }
}

fn select_scrolling_window(windows: Vec<Window>, display: &Display) -> Option<Window> {
    windows.into_iter().find(|window| {
        window.is_visible
            && window.display == display.id
            && !window.bounds.is_empty()
            && !is_capture_control_window(window)
    })
}

fn is_capture_control_window(window: &Window) -> bool {
    window
        .application
        .as_deref()
        .is_some_and(|application| application.eq_ignore_ascii_case("scrozz"))
}

fn intersect_rect(left: LogicalRect, right: LogicalRect) -> Option<LogicalRect> {
    let x = left.origin.x.max(right.origin.x);
    let y = left.origin.y.max(right.origin.y);
    let right_edge = (left.origin.x + left.size.width).min(right.origin.x + right.size.width);
    let bottom = (left.origin.y + left.size.height).min(right.origin.y + right.size.height);
    (right_edge > x && bottom > y).then(|| {
        LogicalRect::new(
            LogicalPoint::new(x, y),
            LogicalSize::new(right_edge - x, bottom - y),
        )
    })
}

fn scroll_session_config(
    display: &Display,
    area: LogicalRect,
    axis: ScrollAxis,
    window: WindowId,
) -> CliResult<ScrollSessionConfig> {
    if area.is_empty() {
        return Err(CliError::Core(CoreError::InvalidRequest(format!(
            "display {} has no usable scrolling viewport",
            display.id.0
        ))));
    }
    let at = scrozz_core::LogicalPoint::new(
        area.origin.x + area.size.width / 2.0,
        area.origin.y + area.size.height / 2.0,
    );
    // Keeping roughly a third of the viewport as overlap gives the matcher real
    // evidence while still making useful progress on each capture.
    let gesture = match axis {
        ScrollAxis::Vertical => ScrollGesture::down(at, area.size.height * 0.65),
        ScrollAxis::Horizontal => ScrollGesture::right(at, area.size.width * 0.65),
    }
    .on_display(display.id.clone())
    .in_window(window);
    Ok(ScrollSessionConfig::new(gesture))
}

// ---------------------------------------------------------------------------
// record
// ---------------------------------------------------------------------------

fn record(args: &RecordArgs) -> CliResult<Report> {
    if args.stop {
        // Reaching here means no instance was running, because a running one
        // would have handled it. There is no session in this process to stop.
        return Err(CliError::Core(CoreError::InvalidRequest(
            "no recording is in progress; `record --stop` talks to the running \
             Scrozz, and nothing is running"
                .to_string(),
        )));
    }

    let target = args.target.resolve()?;
    let plan = Json::obj([
        ("target", target_json(&target)),
        ("fps", Json::Int(args.fps.into())),
        ("microphone", Json::Bool(args.microphone)),
        ("system_audio", Json::Bool(args.system_audio)),
        ("cursor", Json::Bool(args.cursor)),
        ("output", Json::opt(args.output.as_deref(), path_json)),
    ]);

    if args.dry_run {
        return Ok(Report::new(
            Json::obj([("dry_run", Json::Bool(true)), ("plan", plan)]),
            format!(
                "Would record {} at {} fps.",
                describe_target(&target),
                args.fps
            ),
        ));
    }

    let request = scrozz_record::RecordingRequest {
        target: capture_target(&target)?,
        microphone: args.microphone,
        system_audio: args.system_audio,
        fps: args.fps,
        show_cursor: args.cursor,
    };
    let _session = platform::start_recording(&request)?;

    Err(CliError::not_implemented(
        "recording the screen",
        "scrozz-record",
    ))
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
            // protocol. Saying so precisely — and naming the alternative that
            // does work — is the difference between a documented boundary and an
            // app that looks broken.
            if is_wayland() {
                return Err(CliError::Core(CoreError::Unsupported {
                    what: "listing windows".to_string(),
                    why: "Wayland has no window enumeration protocol: a client \
                          cannot see other clients' windows, by design. Use \
                          `scrozz capture --interactive window`, which asks the \
                          compositor's own picker to choose one."
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
pub(crate) fn is_wayland() -> bool {
    is_wayland_environment(
        std::env::var("SCROZZ_BACKEND").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
    )
}

fn is_wayland_environment(
    forced_backend: Option<&str>,
    wayland_display: Option<&str>,
    session_type: Option<&str>,
) -> bool {
    match forced_backend.map(str::trim).map(str::to_ascii_lowercase) {
        Some(forced) if forced == "x11" || forced == "xcb" => return false,
        Some(forced) if forced == "wayland" => return true,
        _ => {}
    }
    wayland_display.is_some_and(|value| !value.is_empty())
        || session_type.is_some_and(|value| value.eq_ignore_ascii_case("wayland"))
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

fn history(command: &HistoryCommand) -> CliResult<Report> {
    match command {
        HistoryCommand::List { .. } => {
            let _store = platform::store()?;
            Err(CliError::not_implemented(
                "listing the capture history",
                "scrozz-store",
            ))
        }
        HistoryCommand::Get { .. } => Err(CliError::not_implemented(
            "reading a stored capture",
            "scrozz-store (the Store trait has list, set_pinned and \
             enforce_retention, but no way to read a capture back)",
        )),
        HistoryCommand::Delete { .. } => Err(CliError::not_implemented(
            "deleting a stored capture",
            "scrozz-store (the Store trait exposes no delete)",
        )),
        HistoryCommand::Pin { .. } => {
            let _store = platform::store()?;
            Err(CliError::not_implemented(
                "pinning a capture",
                "scrozz-store",
            ))
        }
    }
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
            why: "this build has no OCR engine. macOS uses Vision and Windows \
                  uses Windows.Media.Ocr, both supplied by the system; Linux has \
                  no system recogniser, and bundling one would add tens of \
                  megabytes of language model to every install."
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
            let blocks = platform::ocr_engine().recognize(&frame)?;
            Ok(ocr_report(&blocks, &path.display().to_string()))
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
fn ocr_report(blocks: &[scrozz_ocr::TextBlock], source: &str) -> Report {
    let text = blocks
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let data = Json::obj([
        ("source", Json::str(source)),
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
            Json::obj([("settings", settings::all_json())]),
            settings::all_human(),
        )),

        SettingsCommand::Get { key: Some(key) } => {
            let setting = settings::lookup(key)?;
            Ok(Report::new(setting.to_json(), setting.default.to_string()))
        }

        SettingsCommand::Set { key, value } => {
            // Validate first and completely. A rejected value must be rejected
            // for the right reason: "that is not a format" is useful, "settings
            // are not implemented" is not, and the user needs to hear the first
            // one even while the second is true.
            let setting = settings::lookup(key)?;
            setting.validate(value)?;
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
            ("selector", Json::str(display_selector_slug(selector))),
        ]),
        TargetSpec::Scrolling { display, axis } => Json::obj([
            ("kind", Json::str("scrolling")),
            ("selector", Json::str(display_selector_slug(display))),
            ("axis", Json::str(scroll_axis_slug(*axis))),
        ]),
        TargetSpec::AllDisplays => Json::obj([("kind", Json::str("all-displays"))]),
        TargetSpec::Interactive(mode) => Json::obj([
            ("kind", Json::str("interactive")),
            ("mode", Json::str(interactive_slug(*mode))),
        ]),
    }
}

fn display_selector_slug(selector: &DisplaySelector) -> &str {
    match selector {
        DisplaySelector::Primary => "primary",
        DisplaySelector::Active => "active",
        DisplaySelector::Id(id) => id.as_str(),
    }
}

const fn scroll_axis_slug(axis: ScrollAxis) -> &'static str {
    match axis {
        ScrollAxis::Vertical => "vertical",
        ScrollAxis::Horizontal => "horizontal",
    }
}

/// The stable slug for an interactive mode.
#[must_use]
pub const fn interactive_slug(mode: InteractiveMode) -> &'static str {
    match mode {
        InteractiveMode::Region => "region",
        InteractiveMode::Window => "window",
        InteractiveMode::Display => "display",
    }
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
        TargetSpec::Scrolling { display, axis } => {
            let direction = scroll_axis_slug(*axis);
            match display {
                DisplaySelector::Primary => {
                    format!(
                        "a {direction} scrolling capture of the frontmost window on the primary \
                         display"
                    )
                }
                DisplaySelector::Active => {
                    format!(
                        "a {direction} scrolling capture of the frontmost window on the active \
                         display"
                    )
                }
                DisplaySelector::Id(id) => {
                    format!(
                        "a {direction} scrolling capture of the frontmost window on display {id}"
                    )
                }
            }
        }
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
    use clap::Parser;
    use scrozz_core::{ColorSpace, DisplayId, PixelFormat, ScaleFactor};

    use super::*;
    use crate::{cli::Cli, exit::Exit};

    struct PanicFrameSession;

    impl scrozz_capture::FrameSession for PanicFrameSession {
        fn capture_frame(&mut self) -> scrozz_core::Result<Frame> {
            panic!("a cancelled scrolling source must not request its first frame")
        }

        fn name(&self) -> &str {
            "panic-frame-session"
        }
    }

    fn run(argv: &[&str]) -> CliResult<Report> {
        let cli = Cli::try_parse_from(argv).expect("should parse");
        cli.validate()?;
        dispatch(&cli.command.clone().expect("should have a command"))
    }

    fn json_of(argv: &[&str]) -> String {
        run(argv).expect("should succeed").data.to_compact_string()
    }

    fn fixture_display() -> Display {
        Display {
            id: DisplayId("display-2".to_owned()),
            name: "Fixture display".to_owned(),
            bounds: LogicalRect::new(
                LogicalPoint::new(100.0, 50.0),
                LogicalSize::new(1_200.0, 800.0),
            ),
            work_area: LogicalRect::new(
                LogicalPoint::new(100.0, 70.0),
                LogicalSize::new(1_200.0, 760.0),
            ),
            scale: ScaleFactor::new(1.5),
            is_primary: false,
        }
    }

    fn fixture_window(
        id: &str,
        application: &str,
        display: &str,
        bounds: LogicalRect,
        is_visible: bool,
    ) -> Window {
        Window {
            id: WindowId(id.to_owned()),
            title: Some(format!("{application} window")),
            application: Some(application.to_owned()),
            bounds,
            display: DisplayId(display.to_owned()),
            is_visible,
        }
    }

    #[test]
    fn scrolling_keeps_a_frontmost_terminal_as_the_selected_target() {
        let display = fixture_display();
        let bounds = LogicalRect::new(
            LogicalPoint::new(120.0, 80.0),
            LogicalSize::new(900.0, 700.0),
        );
        let windows = vec![
            fixture_window("terminal", "WindowsTerminal", "display-2", bounds, true),
            fixture_window("hud", "Scrozz", "display-2", bounds, true),
            fixture_window("other-display", "Browser", "display-1", bounds, true),
            fixture_window("minimized", "Browser", "display-2", bounds, false),
            fixture_window("front", "Browser", "display-2", bounds, true),
            fixture_window("back", "Notes", "display-2", bounds, true),
        ];

        let selected =
            select_scrolling_window(windows, &display).expect("an application window is available");
        assert_eq!(selected.id, WindowId("terminal".to_owned()));
    }

    #[test]
    fn scrolling_excludes_only_a_positively_identified_scrozz_window() {
        let display = fixture_display();
        let bounds = display.work_area;
        let windows = vec![
            fixture_window("hud", "sCrOzZ", "display-2", bounds, true),
            fixture_window("target", "PowerShell", "display-2", bounds, true),
        ];

        let selected =
            select_scrolling_window(windows, &display).expect("an application window is available");
        assert_eq!(selected.id, WindowId("target".to_owned()));
    }

    #[test]
    fn scrolling_viewport_is_cropped_to_the_selected_displays_work_area() {
        let display = fixture_display();
        let spanning_window = LogicalRect::new(
            LogicalPoint::new(40.0, 20.0),
            LogicalSize::new(1_000.0, 900.0),
        );

        assert_eq!(
            intersect_rect(spanning_window, display.work_area),
            Some(LogicalRect::new(
                LogicalPoint::new(100.0, 70.0),
                LogicalSize::new(940.0, 760.0),
            ))
        );
    }

    #[test]
    fn scrolling_refresh_keeps_window_identity_and_recomputes_geometry() {
        let initial_display = fixture_display();
        let initial_window = fixture_window(
            "browser-window",
            "Browser",
            "display-2",
            LogicalRect::new(
                LogicalPoint::new(120.0, 80.0),
                LogicalSize::new(900.0, 700.0),
            ),
            true,
        );
        let target = resolved_native_scrolling_target(
            CaptureRequest {
                target: CaptureTarget::Display(initial_display.id.clone()),
                cursor: CursorMode::Hidden,
                include_window_shadow: false,
            },
            initial_display,
            initial_window,
        )
        .expect("initial target");

        let mut moved_display = fixture_display();
        moved_display.id = DisplayId("display-3".to_owned());
        moved_display.bounds.origin.x = 1_300.0;
        moved_display.work_area.origin.x = 1_300.0;
        let moved_bounds = LogicalRect::new(
            LogicalPoint::new(1_340.0, 100.0),
            LogicalSize::new(700.0, 500.0),
        );
        let refreshed = target
            .refresh_from_snapshots(
                vec![
                    fixture_window("other", "Notes", "display-3", moved_bounds, true),
                    fixture_window("browser-window", "Browser", "display-3", moved_bounds, true),
                ],
                vec![moved_display.clone()],
            )
            .expect("same window on its current display");

        assert_eq!(
            refreshed.capture_target(),
            CaptureTarget::Window(WindowId("browser-window".to_owned()))
        );
        let ScrollingContext::Native {
            display,
            viewport,
            window,
            crop,
        } = &refreshed.context
        else {
            panic!("native target became a portal target");
        };
        assert_eq!(display.id, moved_display.id);
        assert_eq!(*viewport, moved_bounds);
        assert_eq!(*window, WindowId("browser-window".to_owned()));
        assert_eq!(crop.map(|crop| crop.source_bounds), Some(moved_bounds));

        let config = refreshed
            .session_config(ScrollAxis::Horizontal)
            .expect("refreshed gesture");
        assert_eq!(config.gesture.display, Some(moved_display.id));
        assert_eq!(config.gesture.at, LogicalPoint::new(1_690.0, 350.0));
    }

    #[test]
    fn scrolling_refresh_fails_closed_when_the_selected_window_vanishes() {
        let display = fixture_display();
        let window = fixture_window(
            "browser-window",
            "Browser",
            "display-2",
            display.work_area,
            true,
        );
        let target = resolved_native_scrolling_target(
            CaptureRequest {
                target: CaptureTarget::Display(display.id.clone()),
                cursor: CursorMode::Hidden,
                include_window_shadow: false,
            },
            display.clone(),
            window,
        )
        .expect("initial target");

        let error = target
            .refresh_from_snapshots(Vec::new(), vec![display])
            .expect_err("a different frontmost window must never replace the selected identity");
        assert!(matches!(
            error,
            CliError::Core(CoreError::TargetGone(message))
                if message.contains("browser-window")
        ));
    }

    #[test]
    fn selected_viewport_is_applied_to_every_captured_frame() {
        let source_bounds =
            LogicalRect::new(LogicalPoint::new(40.0, 20.0), LogicalSize::new(100.0, 50.0));
        let viewport =
            LogicalRect::new(LogicalPoint::new(50.0, 25.0), LogicalSize::new(80.0, 40.0));
        let mut data = Vec::with_capacity(200 * 100 * 4);
        for y in 0..100_u8 {
            for x in 0..200_u8 {
                data.extend_from_slice(&[x, y, 0, 255]);
            }
        }
        let frame = Frame {
            data,
            size: PhysicalSize::new(200.0, 100.0),
            stride: 200 * 4,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::DisplayP3,
            scale: ScaleFactor::new(2.0),
        };

        let cropped = FrameCrop::new(source_bounds, viewport)
            .expect("crop")
            .apply(frame)
            .expect("apply");

        assert_eq!((cropped.width(), cropped.height()), (160, 80));
        assert_eq!(&cropped.data[..4], &[20, 10, 0, 255]);
        assert_eq!(cropped.format, PixelFormat::Rgba8);
        assert_eq!(cropped.color_space, ColorSpace::DisplayP3);
        assert_eq!(cropped.scale, ScaleFactor::new(2.0));
    }

    #[test]
    fn only_x11_scrolling_windows_request_the_enumerated_outer_frame() {
        assert_eq!(
            needs_outer_frame_capture(&WindowId("x11:00abcdef".to_owned())),
            cfg!(target_os = "linux")
        );
        assert!(!needs_outer_frame_capture(&WindowId("macos:42".to_owned())));
        assert!(!needs_outer_frame_capture(&WindowId("win32:42".to_owned())));
    }

    #[test]
    fn cancelled_scrolling_acquisition_stops_before_the_first_frame() {
        let cancellation = scrozz_capture::CaptureCancellation::new();
        cancellation.cancel();
        let mut source = CaptureFrameSource {
            session: Box::new(PanicFrameSession),
            crop: None,
            acquisition_cancellation: cancellation,
        };

        let error = source
            .capture_frame()
            .expect_err("cancel must stop before frame acquisition");
        assert!(error.is_cancellation());
    }

    #[test]
    fn scrolling_gesture_preserves_the_selected_display_and_window() {
        let display = fixture_display();
        let viewport = LogicalRect::new(
            LogicalPoint::new(200.0, 100.0),
            LogicalSize::new(800.0, 600.0),
        );
        let window = WindowId("browser-window".to_owned());

        let config =
            scroll_session_config(&display, viewport, ScrollAxis::Horizontal, window.clone())
                .expect("valid viewport");

        assert_eq!(config.gesture.axis, ScrollAxis::Horizontal);
        assert_eq!(config.gesture.display, Some(display.id));
        assert_eq!(config.gesture.window, Some(window));
        assert_eq!(config.gesture.at, LogicalPoint::new(600.0, 400.0));
        assert_eq!(config.gesture.amount, 520.0);
    }

    #[test]
    fn wayland_manual_mode_needs_no_xwayland_geometry() {
        let target = ScrollingTarget::manual_portal(CaptureRequest {
            target: wayland_portal_picker_target(),
            cursor: CursorMode::Hidden,
            include_window_shadow: false,
        });

        let config = target
            .session_config(ScrollAxis::Horizontal)
            .expect("manual portal target");
        assert_eq!(config.gesture.axis, ScrollAxis::Horizontal);
        assert_eq!(config.gesture.amount, 1.0);
        assert_eq!(config.gesture.display, None);
        assert_eq!(config.gesture.window, None);
    }

    #[test]
    fn wayland_scrolling_uses_only_the_explicit_manual_picker_sentinel() {
        assert_eq!(
            wayland_scrolling_capture_target(&DisplaySelector::Active).expect("portal target"),
            CaptureTarget::Window(WindowId("xdg-desktop-portal-picker".to_owned()))
        );

        for selector in [
            DisplaySelector::Primary,
            DisplaySelector::Id("missing-display".to_owned()),
        ] {
            let error = wayland_scrolling_capture_target(&selector)
                .expect_err("an explicit display selector must not be ignored");
            assert!(matches!(
                error,
                CliError::Core(CoreError::Unsupported { .. })
            ));
        }
    }

    #[test]
    fn forced_x11_routing_overrides_wayland_session_markers() {
        assert!(!is_wayland_environment(
            Some("x11"),
            Some("wayland-0"),
            Some("wayland")
        ));
        assert!(!is_wayland_environment(
            Some(" XCB "),
            Some("wayland-0"),
            Some("wayland")
        ));
        assert!(is_wayland_environment(Some("wayland"), None, Some("x11")));
    }

    #[test]
    fn terminal_abort_remains_authoritative_during_finalization() {
        let state = Arc::new(AtomicU8::new(1));
        let mut keep = Some(TerminalCancellation {
            state: Arc::clone(&state),
            acquisition: scrozz_capture::CaptureCancellation::new(),
        });
        assert!(fail_if_terminal_abort(&mut keep).is_ok());

        state.store(2, Ordering::Release);
        let error = fail_if_terminal_abort(&mut keep).expect_err("abort");
        assert!(matches!(error, CliError::Core(CoreError::Cancelled)));
    }

    #[test]
    fn output_publication_and_abort_have_one_ordered_winner() {
        let state = Arc::new(AtomicU8::new(0));
        let mut cancel = Some(TerminalCancellation {
            state: Arc::clone(&state),
            acquisition: scrozz_capture::CaptureCancellation::new(),
        });
        seal_terminal_output(&mut cancel).expect("publication wins");
        assert_eq!(state.load(Ordering::Acquire), 3);
        assert_eq!(advance_terminal_cancellation(&state), None);
        assert!(fail_if_terminal_abort(&mut cancel).is_ok());

        let state = Arc::new(AtomicU8::new(1));
        assert_eq!(advance_terminal_cancellation(&state), Some(2));
        let mut cancel = Some(TerminalCancellation {
            state,
            acquisition: scrozz_capture::CaptureCancellation::new(),
        });
        assert!(matches!(
            seal_terminal_output(&mut cancel),
            Err(CliError::Core(CoreError::Cancelled))
        ));
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
    fn a_scrolling_dry_run_reports_a_noninteractive_stitched_target() {
        let rendered = json_of(&["scrozz", "capture", "--scrolling=primary", "--dry-run"]);
        assert!(rendered.contains(r#""kind":"scrolling""#), "{rendered}");
        assert!(rendered.contains(r#""selector":"primary""#), "{rendered}");
        assert!(rendered.contains(r#""interactive":false"#), "{rendered}");
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
            vec!["scrozz", "capture", "--scrolling=active", "--dry-run"],
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
    fn stopping_with_nothing_running_explains_itself() {
        let err = run(&["scrozz", "record", "--stop"]).unwrap_err();
        assert_eq!(err.exit(), Exit::InvalidRequest);
        assert!(
            err.to_string().contains("no recording is in progress"),
            "{err}"
        );
    }

    // -- unimplemented paths -----------------------------------------------

    #[test]
    fn every_unimplemented_command_reports_rather_than_panics() {
        let cases = [
            vec!["scrozz", "capture", "--region", "0,0,10,10"],
            vec!["scrozz", "record"],
            vec!["scrozz", "history", "list"],
            vec!["scrozz", "history", "get", "abc"],
            vec!["scrozz", "history", "delete", "abc"],
            vec!["scrozz", "history", "pin", "abc"],
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
    fn the_missing_store_reads_are_named_precisely() {
        // These are the two the Store trait genuinely cannot express today, as
        // opposed to merely lacking an implementation.
        let err = run(&["scrozz", "history", "get", "abc"]).unwrap_err();
        assert!(
            err.to_string().contains("no way to read a capture back"),
            "{err}"
        );

        let err = run(&["scrozz", "history", "delete", "abc"]).unwrap_err();
        assert!(err.to_string().contains("no delete"), "{err}");
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
        // D8 requires the alternative, not just the refusal.
        assert!(text.contains("--interactive window"), "{text}");
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
