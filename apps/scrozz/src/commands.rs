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

use std::{path::Path, sync::Arc};

use clap::Parser as _;
use scrozz_annotate::{Renderer as _, SkiaRenderer};
use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, CursorMode, Display,
    Error as CoreError, Provenance, ScrollGesture, SelectionOptions, SelectionOutcome, SourceApp,
    TargetEnumerator,
};
use scrozz_export::{Clipboard, Encoder, FrameEncoder, ImageFormat, NamingContext};
use scrozz_ocr::Ocr as _;
use scrozz_shell::{
    Notification, RegistrationStatus, autostart::AutostartTarget, url_scheme::SchemeTarget,
};
use scrozz_stitch::{
    BackendFrameSource, CancelSignal, NeverCancel, Progress, ScrollSession, ScrollSessionConfig,
    ThreadPacer,
};
use scrozz_store::{
    CaptureId, CaptureRecord, DocumentState, History as _, ImageState, Page, SearchQuery,
    SqliteStore, Store as _, Timestamp,
};
use scrozz_update::{
    CheckOutcome, InstallPlan, Phase, PinnedKeyRing, UpdateState, Updater, VerifiedUpdate,
};
use semver::Version;

use crate::{
    cli::{
        AutostartCommand, CaptureArgs, Cli, Command, Compositor, DisplaySelector, HistoryCommand,
        HotkeyCommand, InteractiveMode, ListWhat, OcrSubject, RecordArgs, SettingsCommand, Sink,
        SystemCommand, TargetSpec, UpdateCommand, UrlCommand,
    },
    fault::{CliError, CliResult},
    gui::selection::CaptureSelector,
    hotkey_config, ipc,
    json::Json,
    output::CaptureOutput,
    platform,
    report::Report,
    settings_runtime,
    settings_store::{self, SettingsStore},
    system_integration::SystemContext,
    url::UrlAction,
};

/// Runs a command locally.
///
/// # Errors
///
/// Whatever the command produces. Cancellation arrives here as
/// [`scrozz_core::Error::Cancelled`] and is rendered as an outcome, not a fault.
pub fn dispatch(command: &Command) -> CliResult<Report> {
    dispatch_inner(command, None)
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
    dispatch_inner(command, Some(selector))
}

fn dispatch_inner(command: &Command, selector: Option<&dyn CaptureSelector>) -> CliResult<Report> {
    match command {
        Command::Capture(args) => capture(args, selector),
        Command::Record(args) => record(args),
        Command::List(args) => list(args.what),
        Command::History(args) => history(&args.command),
        Command::Ocr(args) => ocr(args),
        Command::Settings(args) => settings_command(&args.command),
        Command::Hotkey(args) => hotkey(&args.command),
        Command::Autostart(args) => autostart(args.command),
        Command::Url(args) => url_command(&args.command),
        Command::Update(args) => update(&args.command),
        Command::System(args) => system(&args.command),
        Command::Gui => gui(),
    }
}

// ---------------------------------------------------------------------------
// capture
// ---------------------------------------------------------------------------

fn capture(args: &CaptureArgs, selector: Option<&dyn CaptureSelector>) -> CliResult<Report> {
    let output = CaptureOutput::load()?;
    capture_with_output(args, &output, selector)
}

fn capture_with_output(
    args: &CaptureArgs,
    output: &CaptureOutput,
    selector: Option<&dyn CaptureSelector>,
) -> CliResult<Report> {
    args.validate()?;
    let requested_target = args.target_spec()?;
    let mut sinks = args.sinks();
    if output.copy_to_clipboard() && !sinks.contains(&Sink::Clipboard) {
        sinks.push(Sink::Clipboard);
    }
    let format = args
        .format
        .map_or_else(|| output.format(), |format| format.to_export());
    let quality = args.quality.unwrap_or(output.quality());
    let cursor = args.cursor || output.include_cursor();
    let window_shadow = !args.no_window_shadow && output.include_window_shadow();
    let selection = args.selection_options(None)?;

    let plan = Json::obj([
        ("target", target_json(&requested_target)),
        (
            "interactive",
            Json::Bool(matches!(requested_target, TargetSpec::Interactive(_))),
        ),
        (
            "selection",
            Json::opt(selection.as_ref(), |options| {
                selection_json(options, args.retake)
            }),
        ),
        ("cursor", Json::Bool(cursor)),
        ("window_shadow", Json::Bool(window_shadow)),
        ("format", Json::str(format_slug(format))),
        (
            "quality",
            Json::opt((format != ImageFormat::Png).then_some(quality), |quality| {
                Json::Int(quality.into())
            }),
        ),
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
    let backend: Arc<dyn CaptureBackend> = Arc::from(platform::capture_backend()?);
    let mut lifecycle = SelectorLifecycle::new(selector);
    let scrolling = matches!(requested_target, TargetSpec::Scrolling(_));
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
        concrete => (
            concrete_capture_target(&concrete, backend.as_ref())?,
            None,
            None,
        ),
    };

    let request = CaptureRequest {
        target,
        cursor: if cursor {
            CursorMode::Visible
        } else {
            CursorMode::Hidden
        },
        include_window_shadow: window_shadow,
    };

    if selection_outcome.is_none()
        && let Some(secs) = args.delay
    {
        std::thread::sleep(std::time::Duration::from_secs_f64(secs));
    }
    if selection_outcome.is_none()
        && let Some(selector) = selector
    {
        selector.begin_capture()?;
    }

    let capture = if scrolling {
        scrolling_capture(backend.as_ref(), request)?
    } else {
        match frozen_capture {
            Some(capture) => capture,
            None => crate::gui::selection::capture_selected(
                backend.as_ref(),
                &request,
                selection_outcome.as_ref(),
            )?,
        }
    };
    lifecycle.finish();
    if let Some(outcome) = selection_outcome.as_ref() {
        remember_selection(outcome, backend.as_ref());
    }
    let frame = &capture.frame;

    let bytes = output
        .encoder(args.quality)
        .encode(frame, format)
        .map_err(CliError::Core)?;

    let mut written = Vec::new();
    let mut raw = None;
    for sink in &sinks {
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
                let path = output.export(
                    &bytes,
                    &NamingContext {
                        width: frame.width(),
                        height: frame.height(),
                        ..NamingContext::now()
                    },
                )?;
                written.push(path.display().to_string());
            }
        }
    }

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
        ("provenance", Json::str(format!("{:?}", capture.provenance))),
        ("source_app", source_app_json(&capture.source_app)),
        (
            "window_shadow",
            Json::opt(capture.window_shadow, Json::Bool),
        ),
        (
            "written",
            Json::arr(written.iter().map(|w| Json::str(w.as_str()))),
        ),
    ]);

    let source = capture
        .source_app
        .badge()
        .map_or_else(String::new, |label| format!(" from {label}"));
    let human = format!(
        "Captured {}×{} at {}× ({} KB){}{}",
        frame.width(),
        frame.height(),
        frame.scale.get(),
        bytes.len() / 1024,
        source,
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

const fn format_slug(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpeg",
        ImageFormat::WebP => "webp",
    }
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

fn concrete_capture_target(
    spec: &TargetSpec,
    enumerator: &dyn TargetEnumerator,
) -> CliResult<CaptureTarget> {
    match spec {
        TargetSpec::Region(rect) => Ok(CaptureTarget::Region(*rect)),
        TargetSpec::AllDisplays => Ok(CaptureTarget::AllDisplays),
        // Resolving a name needs enumeration, so it goes through the same
        // backend the capture will use — an id resolved by a different object
        // is an id that can disagree.
        TargetSpec::Display(sel) | TargetSpec::Scrolling(sel) => {
            let displays = enumerator.displays()?;
            let found = match sel {
                DisplaySelector::Primary => displays.iter().find(|d| d.is_primary),
                // The pointer's display, which is where an overlay should appear.
                DisplaySelector::Active => enumerator
                    .active_display()
                    .ok()
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
            let windows = enumerator.windows()?;
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
        TargetSpec::Interactive(_) => unreachable!("interactive targets are resolved before this"),
    }
}

fn source_app_json(source: &SourceApp) -> Json {
    Json::obj([
        ("name", Json::opt(source.name.as_deref(), Json::str)),
        (
            "identifier",
            Json::opt(source.identifier.as_deref(), Json::str),
        ),
        (
            "window_title",
            Json::opt(source.window_title.as_deref(), Json::str),
        ),
        ("badge", Json::opt(source.badge(), Json::str)),
    ])
}

pub(crate) fn scrolling_capture(
    backend: &dyn CaptureBackend,
    request: CaptureRequest,
) -> CliResult<Capture> {
    scrolling_capture_with(backend, request, &mut NeverCancel, |event| {
        tracing::debug!(?event, "scrolling capture progress")
    })
}

pub(crate) fn scrolling_capture_with<C, F>(
    backend: &dyn CaptureBackend,
    request: CaptureRequest,
    cancel: &mut C,
    progress: F,
) -> CliResult<Capture>
where
    C: CancelSignal,
    F: FnMut(Progress),
{
    let CaptureTarget::Display(display_id) = &request.target else {
        return Err(CliError::Core(CoreError::InvalidRequest(
            "scrolling capture requires one display".to_owned(),
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
    let config = scroll_session_config(&display)?;
    let source = BackendFrameSource::new(backend, request.clone());
    let driver = platform::scroll_driver()?;
    let output = ScrollSession::new(source, driver, ThreadPacer, config).run(cancel, progress)?;
    Ok(output.into_capture(request.target))
}

fn scroll_session_config(display: &Display) -> CliResult<ScrollSessionConfig> {
    let area = display.work_area;
    if area.is_empty() {
        return Err(CliError::Core(CoreError::InvalidRequest(format!(
            "display {} has no usable work area",
            display.id.0
        ))));
    }
    let at = scrozz_core::LogicalPoint::new(
        area.origin.x + area.size.width / 2.0,
        area.origin.y + area.size.height / 2.0,
    );
    // Keeping roughly a third of the viewport as overlap gives the matcher real
    // evidence while still making useful progress on each capture.
    Ok(ScrollSessionConfig::new(ScrollGesture::down(
        at,
        area.size.height * 0.65,
    )))
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

    let enumerator = platform::target_enumerator()?;
    let request = scrozz_record::RecordingRequest {
        target: concrete_capture_target(&target, enumerator.as_ref())?,
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
                    (
                        "application_id",
                        Json::opt(w.application_id.as_deref(), Json::str),
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
    let mut store = platform::store()?;
    history_with_store(&mut store, command)
}

fn history_with_store(store: &mut SqliteStore, command: &HistoryCommand) -> CliResult<Report> {
    match command {
        HistoryCommand::List {
            limit,
            offset,
            kind,
            search,
            app,
            after,
            before,
            pinned,
            images_only,
        } => {
            let mut query = SearchQuery::all()
                .between(after.map(Timestamp), before.map(Timestamp))
                .paged(Page::new(*limit, *offset));
            if let Some(kind) = kind {
                query = query.kind(*kind);
            }
            if let Some(search) = search {
                query = query.text(search);
            }
            if let Some(app) = app {
                query = query.app(app);
            }
            if *pinned {
                query = query.pinned_only();
            }
            if *images_only {
                query = query.images_only();
            }

            let captures = store.search(&query)?;
            let total = store.count_matching(&query)?;
            let data = Json::obj([
                ("total", Json::Int(i64::try_from(total).unwrap_or(i64::MAX))),
                ("count", Json::Int(captures.len() as i64)),
                ("limit", Json::Int(i64::from(*limit))),
                ("offset", Json::Int(i64::from(*offset))),
                (
                    "captures",
                    Json::arr(captures.iter().map(capture_record_json)),
                ),
            ]);
            Ok(Report::new(data, history_table(&captures, total)))
        }
        HistoryCommand::Get { id, output, stdout } => {
            history_get(store, id, output.as_deref(), *stdout)
        }
        HistoryCommand::Delete { ids } => {
            let mut deleted = Vec::new();
            let mut not_found = Vec::new();
            for id in ids {
                if store.delete(&CaptureId(id.clone()))? {
                    deleted.push(id.clone());
                } else {
                    not_found.push(id.clone());
                }
            }
            let count = deleted.len();
            let data = Json::obj([
                (
                    "deleted",
                    Json::arr(deleted.iter().map(|id| Json::str(id.as_str()))),
                ),
                (
                    "not_found",
                    Json::arr(not_found.iter().map(|id| Json::str(id.as_str()))),
                ),
                ("count", Json::Int(count as i64)),
            ]);
            let noun = if count == 1 { "capture" } else { "captures" };
            let mut human = format!("Deleted {count} {noun}.");
            if !not_found.is_empty() {
                human.push_str(&format!(" Not found: {}.", not_found.join(", ")));
            }
            Ok(Report::new(data, human))
        }
        HistoryCommand::Pin { id, unpin } => {
            let capture_id = CaptureId(id.clone());
            if store.record(&capture_id)?.is_none() {
                return Err(history_not_found(id));
            }
            let pinned = !unpin;
            store.set_pinned(&capture_id, pinned)?;
            Ok(Report::new(
                Json::obj([
                    ("id", Json::str(id.as_str())),
                    ("pinned", Json::Bool(pinned)),
                ]),
                format!(
                    "{} capture {id}.",
                    if pinned { "Pinned" } else { "Unpinned" }
                ),
            ))
        }
    }
}

fn history_get(
    store: &mut SqliteStore,
    id: &str,
    output: Option<&Path>,
    stdout: bool,
) -> CliResult<Report> {
    let capture_id = CaptureId(id.to_owned());
    let document = match store.document(&capture_id)? {
        Some(DocumentState::Complete(document)) => document,
        Some(DocumentState::ImageEvicted(_)) => return Err(history_image_evicted(id)),
        None => return Err(history_not_found(id)),
    };
    let frame = SkiaRenderer::new().render(&document)?;
    let width = frame.width();
    let height = frame.height();
    let bytes = FrameEncoder::new().encode(&frame, ImageFormat::Png)?;

    let path = if stdout {
        None
    } else if let Some(path) = output {
        std::fs::write(path, &bytes)?;
        Some(path.to_path_buf())
    } else {
        Some(CaptureOutput::load()?.export(
            &bytes,
            &NamingContext {
                width,
                height,
                ..NamingContext::now()
            },
        )?)
    };
    let human = path.as_ref().map_or_else(
        || format!("Wrote capture {id} to stdout."),
        |path| format!("Saved capture {id} to {}.", path.display()),
    );
    let data = Json::obj([
        ("id", Json::str(id)),
        (
            "path",
            Json::opt(path.as_ref(), |path| path_json(path.as_path())),
        ),
        ("bytes", Json::Int(bytes.len() as i64)),
        ("format", Json::str("png")),
        ("width", Json::Int(i64::from(width))),
        ("height", Json::Int(i64::from(height))),
    ]);
    let report = Report::new(data, human);
    Ok(if stdout {
        report.with_raw(bytes)
    } else {
        report
    })
}

fn capture_record_json(record: &CaptureRecord) -> Json {
    Json::obj([
        ("id", Json::str(record.id.0.as_str())),
        ("created_at", Json::Int(record.created_at.as_millis())),
        ("media_kind", Json::str(record.media_kind.as_token())),
        ("pinned", Json::Bool(record.pinned)),
        (
            "app_name",
            Json::opt(record.source_app.name.as_deref(), Json::str),
        ),
        (
            "app_identifier",
            Json::opt(record.source_app.identifier.as_deref(), Json::str),
        ),
        (
            "window_title",
            Json::opt(record.source_app.window_title.as_deref(), Json::str),
        ),
        ("source_app", source_app_json(&record.source_app)),
        ("window_shadow", Json::opt(record.window_shadow, Json::Bool)),
        ("provenance", Json::str(provenance_slug(record.provenance))),
        ("width", Json::Float(record.frame.size.width)),
        ("height", Json::Float(record.frame.size.height)),
        ("scale", Json::Float(record.frame.scale.get())),
        ("image_state", Json::str(image_state_slug(&record.image))),
        (
            "image_bytes",
            Json::Int(i64::try_from(record.image.byte_len()).unwrap_or(i64::MAX)),
        ),
        (
            "annotation_count",
            Json::Int(record.annotation_count as i64),
        ),
        ("has_ocr", Json::Bool(record.ocr_text.is_some())),
        ("ocr_text", Json::opt(record.ocr_text.as_deref(), Json::str)),
    ])
}

fn history_table(captures: &[CaptureRecord], total: u64) -> String {
    if captures.is_empty() {
        return "No captures matched.".to_owned();
    }
    let mut lines = Vec::with_capacity(captures.len() + 2);
    lines.push("ID\tTIME (UTC)\tKIND\tAPP / TITLE\tPIN\tPIXELS\tEDITS\tOCR".to_owned());
    for record in captures {
        let app_title = match (&record.source_app.name, &record.source_app.window_title) {
            (Some(app), Some(title)) => format!("{app} / {title}"),
            (Some(app), None) => app.clone(),
            (None, Some(title)) => title.clone(),
            (None, None) => "-".to_owned(),
        };
        lines.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            record.id.0,
            format_utc_millis(record.created_at),
            record.media_kind.as_token(),
            app_title,
            if record.pinned { "yes" } else { "no" },
            match &record.image {
                ImageState::Present { byte_len, .. } => format_bytes(*byte_len),
                ImageState::Evicted { .. } => "evicted".to_owned(),
                ImageState::Absent => "absent".to_owned(),
            },
            record.annotation_count,
            if record.ocr_text.is_some() {
                "yes"
            } else {
                "no"
            },
        ));
    }
    lines.push(format!(
        "Showing {} of {total} matching captures.",
        captures.len()
    ));
    lines.join("\n")
}

fn format_utc_millis(timestamp: Timestamp) -> String {
    let millis = timestamp.as_millis();
    let civil = scrozz_export::Timestamp::from_unix_seconds(millis.div_euclid(1_000));
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        civil.year,
        civil.month,
        civil.day,
        civil.hour,
        civil.minute,
        civil.second,
        millis.rem_euclid(1_000)
    )
}

const fn image_state_slug(state: &ImageState) -> &'static str {
    match state {
        ImageState::Present { .. } => "present",
        ImageState::Evicted { .. } => "evicted",
        ImageState::Absent => "absent",
    }
}

const fn provenance_slug(provenance: Provenance) -> &'static str {
    match provenance {
        Provenance::Display => "display",
        Provenance::Window => "window",
        Provenance::Region => "region",
        Provenance::AllDisplays => "all-displays",
        Provenance::Stitched => "stitched",
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn history_not_found(id: &str) -> CliError {
    CliError::Core(CoreError::Storage(format!(
        "capture {id:?} was not found in history"
    )))
}

fn history_image_evicted(id: &str) -> CliError {
    CliError::Core(CoreError::Storage(format!(
        "capture {id:?} is still in history, but its source image was evicted by the retention policy"
    )))
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
            let blocks = confident_blocks(
                platform::ocr_engine().recognize(&frame)?,
                args.min_confidence,
            );
            Ok(ocr_report(&blocks, &path.display().to_string()))
        }
        OcrSubject::Capture(id) => {
            let mut store = platform::store()?;
            ocr_stored_capture(
                &mut store,
                &platform::ocr_engine(),
                &id,
                args.min_confidence,
            )
        }
    }
}

fn ocr_stored_capture(
    store: &mut SqliteStore,
    engine: &impl scrozz_ocr::Ocr,
    id: &str,
    min_confidence: Option<f32>,
) -> CliResult<Report> {
    let capture_id = CaptureId(id.to_owned());
    let document = match store.document(&capture_id)? {
        Some(DocumentState::Complete(document)) => document,
        Some(DocumentState::ImageEvicted(_)) => return Err(history_image_evicted(id)),
        None => return Err(history_not_found(id)),
    };
    let blocks = confident_blocks(engine.recognize(&document.source.frame)?, min_confidence);
    let text = scrozz_ocr::plain_text(&blocks);
    store.set_ocr_text(&capture_id, Some(&text))?;
    Ok(ocr_report(&blocks, id))
}

fn confident_blocks(
    blocks: Vec<scrozz_ocr::TextBlock>,
    min_confidence: Option<f32>,
) -> Vec<scrozz_ocr::TextBlock> {
    let Some(min) = min_confidence else {
        return blocks;
    };
    blocks
        .into_iter()
        .filter(|block| block.confidence >= min)
        .collect()
}

/// Renders recognised text for both `--json` and human output.
///
/// The human rendering is **the text and nothing else**, one block per line, so
/// `scrozz ocr shot.png | pbcopy` does the obvious thing. Bounds and confidence
/// belong in `--json`, where a consumer asked for structure; printing them in
/// the human path would corrupt the far more common case of piping the text
/// somewhere.
fn ocr_report(blocks: &[scrozz_ocr::TextBlock], source: &str) -> Report {
    let text = scrozz_ocr::plain_text(blocks);

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
        SettingsCommand::Get { key: None } => {
            let store = SettingsStore::load()?;
            Ok(Report::new(
                Json::obj([("settings", store.all_json())]),
                store.all_human(),
            ))
        }

        SettingsCommand::Get { key: Some(key) } => {
            let store = SettingsStore::load()?;
            let (setting, value, source) = store.get(key)?;
            Ok(Report::new(
                setting.to_json(value, source),
                value.to_owned(),
            ))
        }

        SettingsCommand::Set { key, value } => {
            let mut store = SettingsStore::load()?;
            settings_runtime::set(&mut store, key, value)?;
            let (setting, resolved, source) = store.get(key)?;
            Ok(Report::new(
                setting.to_json(resolved, source),
                format!("{key} = {resolved}"),
            ))
        }

        SettingsCommand::Reset { key: Some(key) } => {
            let mut store = SettingsStore::load()?;
            settings_runtime::reset(&mut store, key)?;
            let (setting, value, source) = store.get(key)?;
            Ok(Report::new(
                setting.to_json(value, source),
                format!("{key} = {value}"),
            ))
        }

        SettingsCommand::Reset { key: None } => {
            let mut store = SettingsStore::load()?;
            let count = settings_runtime::reset_all(&mut store)?;
            Ok(Report::new(
                Json::obj([
                    ("reset", Json::Int(i64::try_from(count).unwrap_or(i64::MAX))),
                    (
                        "path",
                        Json::str(store.path().to_string_lossy().into_owned()),
                    ),
                ]),
                format!("Reset {count} setting override(s)."),
            ))
        }

        SettingsCommand::Path => {
            let path = settings_store::settings_path()?;
            let text = path.to_string_lossy().into_owned();
            Ok(Report::new(
                Json::obj([("path", Json::str(text.as_str()))]),
                text,
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

// ---------------------------------------------------------------------------
// system integration
// ---------------------------------------------------------------------------

fn autostart(command: AutostartCommand) -> CliResult<Report> {
    let context = SystemContext::current()?;
    let plan = context.autostart()?;
    match command {
        AutostartCommand::Status => {
            let status = plan.status()?;
            Ok(registration_report(
                "autostart",
                status,
                autostart_target(plan.target()),
            ))
        }
        AutostartCommand::Enable => {
            plan.apply()?;
            let status = plan.status()?;
            Ok(registration_report(
                "autostart",
                status,
                autostart_target(plan.target()),
            ))
        }
        AutostartCommand::Disable => {
            plan.remove()?;
            let status = plan.status()?;
            Ok(registration_report(
                "autostart",
                status,
                autostart_target(plan.target()),
            ))
        }
    }
}

fn url_command(command: &UrlCommand) -> CliResult<Report> {
    const TOGGLE: &str = "system.url-scheme-enabled";

    let mut store = SettingsStore::load()?;
    match command {
        UrlCommand::Status => {
            let context = SystemContext::current()?;
            let plan = context.url_scheme()?;
            let status = plan.status()?;
            let enabled = store.boolean(TOGGLE)?;
            Ok(Report::new(
                Json::obj([
                    ("registered", Json::str(registration_slug(status))),
                    ("enabled", Json::Bool(enabled)),
                    ("target", Json::str(scheme_target(plan.target()))),
                ]),
                format!(
                    "URL registration: {}\nURL automation: {}\nTarget: {}",
                    registration_slug(status),
                    if enabled { "enabled" } else { "disabled" },
                    scheme_target(plan.target())
                ),
            ))
        }
        UrlCommand::Register => {
            let context = SystemContext::current()?;
            let plan = context.url_scheme()?;
            plan.apply()?;
            let status = plan.status()?;
            let enabled = store.boolean(TOGGLE)?;
            Ok(Report::new(
                Json::obj([
                    ("registered", Json::str(registration_slug(status))),
                    ("enabled", Json::Bool(enabled)),
                    ("target", Json::str(scheme_target(plan.target()))),
                ]),
                format!(
                    "Registered {}. URL automation remains {}.",
                    scheme_target(plan.target()),
                    if enabled { "enabled" } else { "disabled" }
                ),
            ))
        }
        UrlCommand::Unregister => {
            set_url_enabled(&mut store, false)?;
            let context = SystemContext::current()?;
            let plan = context.url_scheme()?;
            plan.remove()?;
            let status = plan.status()?;
            Ok(Report::new(
                Json::obj([
                    ("registered", Json::str(registration_slug(status))),
                    ("enabled", Json::Bool(false)),
                    ("target", Json::str(scheme_target(plan.target()))),
                ]),
                format!(
                    "URL automation disabled. URL registration is {}.",
                    registration_slug(status)
                ),
            ))
        }
        UrlCommand::Enable => {
            let value = set_url_enabled(&mut store, true)?;
            Ok(Report::new(
                Json::obj([("enabled", Json::Bool(true)), ("setting", value)]),
                "URL automation enabled for allow-listed actions.",
            ))
        }
        UrlCommand::Disable => {
            let value = set_url_enabled(&mut store, false)?;
            Ok(Report::new(
                Json::obj([("enabled", Json::Bool(false)), ("setting", value)]),
                "URL automation disabled.",
            ))
        }
        UrlCommand::Handle { url } => {
            let action = enabled_url_action(&store, url)?;
            dispatch_url_action(action)
        }
    }
}

pub(crate) fn enabled_url_action(store: &SettingsStore, url: &str) -> CliResult<UrlAction> {
    if !store.boolean("system.url-scheme-enabled")? {
        return Err(CliError::Core(CoreError::InvalidRequest(
            "URL automation is disabled; run `scrozz url enable` to grant consent".to_owned(),
        )));
    }
    UrlAction::parse(url)
}

fn dispatch_url_action(action: UrlAction) -> CliResult<Report> {
    let command = command_for_url_action(action)?;
    match ipc::probe() {
        ipc::Status::Running => {
            let response = ipc::forward_url(action)?;
            if response.code != 0 {
                return Err(CliError::ipc(format!(
                    "the running Scrozz rejected {} (exit {}): {}",
                    action.slug(),
                    response.code,
                    String::from_utf8_lossy(&response.payload).trim()
                )));
            }
            Ok(Report::new(
                Json::obj([
                    ("action", Json::str(action.slug())),
                    ("forwarded", Json::Bool(true)),
                ]),
                format!("Forwarded {} to the running Scrozz.", action.slug()),
            ))
        }
        ipc::Status::NotRunning if ipc::policy(&command) == ipc::Forwarding::Require => {
            Err(CliError::ipc(format!(
                "{} requires a running Scrozz instance",
                action.slug()
            )))
        }
        ipc::Status::NotRunning => dispatch(&command),
        ipc::Status::Unusable(reason) => Err(CliError::ipc(format!(
            "refusing URL action because the Scrozz IPC endpoint is unusable: {reason}"
        ))),
    }
}

pub(crate) fn command_for_url_action(action: UrlAction) -> CliResult<Command> {
    let mut argv = Vec::with_capacity(action.arguments().len() + 1);
    argv.push("scrozz");
    argv.extend(action.arguments().iter().copied());
    Cli::try_parse_from(argv)
        .map_err(|error| {
            CliError::Core(CoreError::Platform(format!(
                "allow-listed URL action {} maps to an invalid command: {error}",
                action.slug()
            )))
        })?
        .command
        .ok_or_else(|| {
            CliError::Core(CoreError::Platform(format!(
                "allow-listed URL action {} maps to no command",
                action.slug()
            )))
        })
}

fn set_url_enabled(store: &mut SettingsStore, enabled: bool) -> CliResult<Json> {
    const KEY: &str = "system.url-scheme-enabled";
    settings_runtime::set(store, KEY, if enabled { "true" } else { "false" })?;
    let (setting, value, source) = store.get(KEY)?;
    Ok(setting.to_json(value, source))
}

fn update(command: &UpdateCommand) -> CliResult<Report> {
    let context = SystemContext::current()?;
    let keys = PinnedKeyRing::production();

    if matches!(command, UpdateCommand::Check { .. }) && keys.is_empty() {
        return Err(CliError::not_implemented(
            "checking release manifests until a human-controlled public key is pinned",
            "the release signing process",
        ));
    }

    let mut updater = Updater::open(context.update_state.clone(), keys).map_err(update_error)?;
    match command {
        UpdateCommand::Status => Ok(update_state_report(
            updater.state(),
            &context.update_state,
            PinnedKeyRing::production().len(),
        )),
        UpdateCommand::Check {
            manifest_url,
            signature_url,
        } => {
            let installed = Version::parse(scrozz_core::identity::VERSION).map_err(|error| {
                CliError::Core(CoreError::Platform(format!(
                    "this build has an invalid semantic version: {error}"
                )))
            })?;
            let outcome = updater
                .check(manifest_url.clone(), signature_url.clone(), &installed)
                .map_err(update_error)?;
            Ok(check_outcome_report(&outcome))
        }
        UpdateCommand::Download { output } => {
            let candidate = if updater.state().phase() == Phase::Failed {
                updater.retry_available_update().map_err(update_error)?
            } else {
                updater.available_update().ok_or_else(|| {
                    CliError::usage(
                        "no verified update is available; run `scrozz update check` first",
                    )
                })?
            };
            let download = updater
                .download(&candidate, output.clone())
                .map_err(update_error)?;
            Ok(Report::new(
                Json::obj([
                    ("phase", Json::str("downloaded")),
                    (
                        "path",
                        Json::str(download.path().to_string_lossy().into_owned()),
                    ),
                    ("candidate", candidate_json(&candidate)),
                ]),
                format!("Verified update artifact at {}.", download.path().display()),
            ))
        }
        UpdateCommand::Stage { output } => {
            let download = updater.downloaded_artifact().map_err(update_error)?;
            let staged = updater
                .stage(&download, output.clone())
                .map_err(update_error)?;
            Ok(Report::new(
                Json::obj([
                    ("phase", Json::str("staged")),
                    (
                        "path",
                        Json::str(staged.path().to_string_lossy().into_owned()),
                    ),
                ]),
                format!(
                    "Staged verified artifact at {}. It has not been installed.",
                    staged.path().display()
                ),
            ))
        }
        UpdateCommand::Install {
            installed,
            previous,
            failed_candidate,
        } => {
            let staged = updater.staged_artifact().map_err(update_error)?;
            let plan = InstallPlan::new(
                installed.clone(),
                previous.clone(),
                failed_candidate.clone(),
            )
            .map_err(update_error)?;
            updater.install(&staged, plan).map_err(update_error)?;
            Ok(Report::new(
                Json::obj([
                    ("phase", Json::str("installed")),
                    (
                        "installed",
                        Json::str(installed.to_string_lossy().into_owned()),
                    ),
                    (
                        "previous",
                        Json::str(previous.to_string_lossy().into_owned()),
                    ),
                ]),
                format!(
                    "Installed verified artifact at {} and retained {}.",
                    installed.display(),
                    previous.display()
                ),
            ))
        }
        UpdateCommand::Recover => {
            updater.recover().map_err(update_error)?;
            Ok(update_state_report(
                updater.state(),
                &context.update_state,
                PinnedKeyRing::production().len(),
            ))
        }
        UpdateCommand::Rollback => {
            updater.rollback().map_err(update_error)?;
            Ok(update_state_report(
                updater.state(),
                &context.update_state,
                PinnedKeyRing::production().len(),
            ))
        }
        UpdateCommand::Reset => {
            updater.reset_to_idle().map_err(update_error)?;
            Ok(update_state_report(
                updater.state(),
                &context.update_state,
                PinnedKeyRing::production().len(),
            ))
        }
    }
}

fn system(command: &SystemCommand) -> CliResult<Report> {
    match command {
        SystemCommand::Status => {
            let context = SystemContext::current()?;
            let autostart = context.autostart()?;
            let scheme = context.url_scheme()?;
            let autostart_status = autostart.status()?;
            let scheme_status = scheme.status()?;
            let url_enabled =
                SettingsStore::open_default()?.boolean("system.url-scheme-enabled")?;
            let updater = Updater::open_with_production_keys(context.update_state.clone())
                .map_err(update_error)?;
            let trusted_update_keys = PinnedKeyRing::production().len();
            Ok(Report::new(
                Json::obj([
                    ("product", Json::str(scrozz_core::identity::PRODUCT_NAME)),
                    ("version", Json::str(scrozz_core::identity::VERSION)),
                    ("bundle_id", Json::str(scrozz_core::identity::BUNDLE_ID)),
                    ("url_scheme", Json::str(scrozz_core::identity::URL_SCHEME)),
                    ("platform", Json::str(context.platform.slug())),
                    ("package_kind", Json::str(context.package_kind.slug())),
                    (
                        "platform_key",
                        Json::str(scrozz_core::identity::platform_key()),
                    ),
                    ("autostart", Json::str(registration_slug(autostart_status))),
                    (
                        "url_registration",
                        Json::str(registration_slug(scheme_status)),
                    ),
                    ("url_enabled", Json::Bool(url_enabled)),
                    (
                        "update_phase",
                        Json::str(phase_slug(updater.state().phase())),
                    ),
                    ("trusted_update_keys", Json::Int(trusted_update_keys as i64)),
                ]),
                format!(
                    "{} {} ({})\nPackage: {}\nAutostart: {}\nURL registration: {}\nURL automation: {}\nUpdate state: {}\nTrusted update keys: {}",
                    scrozz_core::identity::PRODUCT_NAME,
                    scrozz_core::identity::VERSION,
                    scrozz_core::identity::platform_key(),
                    context.package_kind.slug(),
                    registration_slug(autostart_status),
                    registration_slug(scheme_status),
                    if url_enabled { "enabled" } else { "disabled" },
                    phase_slug(updater.state().phase()),
                    trusted_update_keys_summary(trusted_update_keys),
                ),
            ))
        }
        SystemCommand::Notify { title, body } => {
            let platform = scrozz_shell::SystemPlatform::current()?;
            let notification = Notification::new(title.clone(), body.clone())?;
            notification.plan(platform).apply()?;
            Ok(Report::new(
                Json::obj([
                    ("shown", Json::Bool(true)),
                    ("platform", Json::str(platform.slug())),
                ]),
                "Notification shown.",
            ))
        }
    }
}

fn registration_report(kind: &str, status: RegistrationStatus, target: String) -> Report {
    Report::new(
        Json::obj([
            ("kind", Json::str(kind)),
            ("status", Json::str(registration_slug(status))),
            ("target", Json::str(target.clone())),
        ]),
        format!(
            "{}: {}\nTarget: {target}",
            if kind == "autostart" {
                "Autostart"
            } else {
                "Registration"
            },
            registration_slug(status)
        ),
    )
}

const fn registration_slug(status: RegistrationStatus) -> &'static str {
    match status {
        RegistrationStatus::Disabled => "disabled",
        RegistrationStatus::Enabled => "enabled",
        RegistrationStatus::Drifted => "drifted",
    }
}

fn autostart_target(target: &AutostartTarget) -> String {
    match target {
        AutostartTarget::File(path) => path.to_string_lossy().into_owned(),
        AutostartTarget::RegistryValue { key, name } => format!("{key}\\{name}"),
        AutostartTarget::PackageStartupTask { task_id } => {
            format!("MSIX startup task {task_id}")
        }
    }
}

fn scheme_target(target: &SchemeTarget) -> String {
    match target {
        SchemeTarget::ApplicationBundle(path) | SchemeTarget::DesktopFile(path) => {
            path.to_string_lossy().into_owned()
        }
        SchemeTarget::RegistryClass(key) => key.clone(),
        SchemeTarget::PackageManifest { scheme } => {
            format!("MSIX manifest protocol {scheme}://")
        }
    }
}

fn update_state_report(state: &UpdateState, state_path: &Path, trusted_keys: usize) -> Report {
    let candidate = state.candidate().map(|candidate| {
        Json::obj([
            ("version", Json::str(candidate.version().to_string())),
            ("generation", Json::str(candidate.generated().to_string())),
        ])
    });
    let artifact = state.artifact().map(|artifact| {
        Json::obj([
            ("platform", Json::str(artifact.platform())),
            ("url", Json::str(artifact.url().as_str())),
            ("sha256", Json::str(artifact.sha256().as_hex())),
            ("size", Json::str(artifact.size().to_string())),
        ])
    });
    let plan = state.install_plan().map(|plan| {
        Json::obj([
            (
                "installed",
                Json::str(plan.installed().to_string_lossy().into_owned()),
            ),
            (
                "previous",
                Json::str(plan.previous().to_string_lossy().into_owned()),
            ),
            (
                "failed_candidate",
                Json::str(plan.failed_candidate().to_string_lossy().into_owned()),
            ),
        ])
    });
    let human = format!(
        "Update state: {}\nGeneration watermark: {}\nState file: {}\nTrusted update keys: {}{}",
        phase_slug(state.phase()),
        state.highest_accepted_generation(),
        state_path.display(),
        trusted_keys,
        state
            .failure()
            .map_or_else(String::new, |failure| format!("\nFailure: {failure}"))
    );
    Report::new(
        Json::obj([
            ("phase", Json::str(phase_slug(state.phase()))),
            (
                "highest_accepted_generation",
                Json::str(state.highest_accepted_generation().to_string()),
            ),
            ("candidate", Json::opt(candidate, |value| value)),
            ("artifact", Json::opt(artifact, |value| value)),
            (
                "downloaded_path",
                Json::opt(state.downloaded_path(), |path| {
                    Json::str(path.to_string_lossy().into_owned())
                }),
            ),
            (
                "staged_path",
                Json::opt(state.staged_path(), |path| {
                    Json::str(path.to_string_lossy().into_owned())
                }),
            ),
            ("install_plan", Json::opt(plan, |value| value)),
            ("rollback_requested", Json::Bool(state.rollback_requested())),
            (
                "failure",
                Json::opt(state.failure(), |failure| Json::str(failure.to_owned())),
            ),
            (
                "state_file",
                Json::str(state_path.to_string_lossy().into_owned()),
            ),
            ("trusted_keys", Json::Int(trusted_keys as i64)),
        ]),
        human,
    )
}

fn check_outcome_report(outcome: &CheckOutcome) -> Report {
    match outcome {
        CheckOutcome::Current { version, generated } => Report::new(
            Json::obj([
                ("outcome", Json::str("current")),
                ("version", Json::str(version.to_string())),
                ("generation", Json::str(generated.to_string())),
            ]),
            format!("Scrozz {version} is current."),
        ),
        CheckOutcome::PlatformUnavailable {
            version,
            generated,
            platform,
        } => Report::new(
            Json::obj([
                ("outcome", Json::str("platform-unavailable")),
                ("version", Json::str(version.to_string())),
                ("generation", Json::str(generated.to_string())),
                ("platform", Json::str(platform)),
            ]),
            format!("Scrozz {version} is signed, but no {platform} artifact is published."),
        ),
        CheckOutcome::UpdateAvailable(candidate) => Report::new(
            Json::obj([
                ("outcome", Json::str("update-available")),
                ("candidate", candidate_json(candidate)),
                ("installed", Json::Bool(false)),
            ]),
            format!(
                "Scrozz {} is available. No artifact was downloaded or installed.",
                candidate.version()
            ),
        ),
    }
}

fn candidate_json(candidate: &VerifiedUpdate) -> Json {
    let artifact = candidate.artifact().metadata();
    Json::obj([
        ("version", Json::str(candidate.version().to_string())),
        ("generation", Json::str(candidate.generated().to_string())),
        ("platform", Json::str(artifact.platform())),
        ("url", Json::str(artifact.url().as_str())),
        ("sha256", Json::str(artifact.sha256().as_hex())),
        ("size", Json::str(artifact.size().to_string())),
    ])
}

const fn phase_slug(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "idle",
        Phase::Checking => "checking",
        Phase::UpdateAvailable => "update-available",
        Phase::Downloading => "downloading",
        Phase::Downloaded => "downloaded",
        Phase::Staged => "staged",
        Phase::AwaitingRestart => "awaiting-restart",
        Phase::Installed => "installed",
        Phase::Failed => "failed",
        Phase::RolledBack => "rolled-back",
    }
}

fn trusted_update_keys_summary(count: usize) -> String {
    if count == 0 {
        "0 (release signing gated)".to_owned()
    } else {
        count.to_string()
    }
}

fn update_error(error: scrozz_update::Error) -> CliError {
    CliError::Core(CoreError::Platform(format!(
        "signed update failed: {error}"
    )))
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
        TargetSpec::Scrolling(selector) => Json::obj([
            ("kind", Json::str("scrolling")),
            ("selector", Json::str(display_selector_slug(selector))),
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
        TargetSpec::Scrolling(DisplaySelector::Primary) => {
            "a scrolling capture of the primary display".to_owned()
        }
        TargetSpec::Scrolling(DisplaySelector::Active) => {
            "a scrolling capture of the active display".to_owned()
        }
        TargetSpec::Scrolling(DisplaySelector::Id(id)) => {
            format!("a scrolling capture of display {id}")
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
    if matches!(
        command,
        Command::Capture(args)
            if args.target.interactive == Some(InteractiveMode::Window)
    ) {
        // A forwarded command is served synchronously on the existing eframe
        // main thread. Starting a second native event loop there is invalid, so
        // the one-shot CLI owns its picker. Tray and hotkey window captures use
        // the GUI pipeline and its child viewport instead.
        return ipc::Forwarding::Never;
    }
    ipc::policy(command)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use clap::Parser;
    use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};
    use scrozz_store::{
        MediaKind, NewCapture, RetentionPolicy, RetentionWindow,
        test_support::{ScratchDir, id_at, sample_document, scratch_dir},
    };

    use super::*;
    use crate::{cli::Cli, exit::Exit};

    fn run(argv: &[&str]) -> CliResult<Report> {
        let cli = Cli::try_parse_from(argv).expect("should parse");
        cli.validate()?;
        let command = cli.command.clone().expect("should have a command");
        match &command {
            Command::Capture(args) => capture_with_output(args, &default_capture_output(), None),
            _ => dispatch(&command),
        }
    }

    fn default_capture_output() -> CaptureOutput {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "scrozz-command-capture-defaults-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let store = SettingsStore::open(path).unwrap();
        CaptureOutput::from_store(&store).unwrap()
    }

    fn json_of(argv: &[&str]) -> String {
        run(argv).expect("should succeed").data.to_compact_string()
    }

    fn with_settings<T>(name: &str, body: impl FnOnce() -> T) -> T {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let _env = crate::test_env::lock();
        let directory = std::env::temp_dir().join(format!(
            "scrozz-command-settings-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        crate::test_env::set(settings_store::CONFIG_DIR_ENV, directory.to_str().unwrap());
        let result = body();
        let _ = std::fs::remove_dir_all(directory);
        result
    }

    fn history_fixture() -> (ScratchDir, SqliteStore, CaptureId, CaptureId) {
        let dir = scratch_dir("commands-history");
        let mut store = SqliteStore::open_ephemeral(dir.path()).expect("open history");
        let first_document = sample_document(16, 8, 1, 2);
        let first = store
            .insert(
                NewCapture::new(&first_document)
                    .from_app("Preview")
                    .titled("January invoice")
                    .with_ocr("Total due")
                    .taken_at(Timestamp(1_735_776_000_000))
                    .pinned(),
            )
            .expect("insert screenshot");
        let second_document = sample_document(12, 6, 2, 0);
        let second = store
            .insert(
                NewCapture::of_kind(&second_document, MediaKind::Video)
                    .from_app("Safari")
                    .titled("Product demo")
                    .taken_at(Timestamp(1_735_862_400_000)),
            )
            .expect("insert video");
        (dir, store, first, second)
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
    fn an_unset_quality_is_null_for_the_default_lossless_format() {
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
    fn history_list_filters_paginates_and_keeps_json_key_order_stable() {
        let (_dir, mut store, first, _) = history_fixture();
        let report = history_with_store(
            &mut store,
            &HistoryCommand::List {
                limit: 1,
                offset: 0,
                kind: Some(MediaKind::Screenshot),
                search: Some("invoice".into()),
                app: Some("preview".into()),
                after: Some(1_735_689_600_000),
                before: Some(1_735_862_399_999),
                pinned: true,
                images_only: true,
            },
        )
        .expect("list history");
        let Json::Obj(fields) = report.data else {
            panic!("history list must return an object")
        };
        assert_eq!(
            fields
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            ["total", "count", "limit", "offset", "captures"]
        );
        assert_eq!(fields[0].1, Json::Int(1));
        let Json::Arr(captures) = &fields[4].1 else {
            panic!("captures must be an array")
        };
        let Json::Obj(capture) = &captures[0] else {
            panic!("capture must be an object")
        };
        assert_eq!(
            capture
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            [
                "id",
                "created_at",
                "media_kind",
                "pinned",
                "app_name",
                "app_identifier",
                "window_title",
                "source_app",
                "window_shadow",
                "provenance",
                "width",
                "height",
                "scale",
                "image_state",
                "image_bytes",
                "annotation_count",
                "has_ocr",
                "ocr_text",
            ]
        );
        assert_eq!(capture[0].1, Json::str(first.0));
        assert!(report.human.contains("January invoice"));
    }

    #[test]
    fn history_get_renders_the_stored_document_to_png() {
        let (_dir, mut store, first, _) = history_fixture();
        let report = history_get(&mut store, &first.0, None, true).expect("read stored screenshot");
        let bytes = report.raw.expect("stdout mode returns bytes");
        assert_eq!(ImageFormat::sniff(&bytes), Some(ImageFormat::Png));
        assert!(report.data.to_compact_string().contains(r#""path":null"#));
    }

    #[test]
    fn history_pin_and_delete_mutate_the_durable_record() {
        let (_dir, mut store, _, second) = history_fixture();
        history_with_store(
            &mut store,
            &HistoryCommand::Pin {
                id: second.0.clone(),
                unpin: false,
            },
        )
        .expect("pin capture");
        assert!(store.record(&second).unwrap().unwrap().pinned);

        let missing = id_at(1_600_000_000_000);
        let report = history_with_store(
            &mut store,
            &HistoryCommand::Delete {
                ids: vec![second.0.clone(), missing.0.clone()],
            },
        )
        .expect("delete capture");
        assert!(store.record(&second).unwrap().is_none());
        let rendered = report.data.to_compact_string();
        assert!(rendered.contains(r#""count":1"#), "{rendered}");
        assert!(
            rendered.contains(&format!(r#""not_found":["{}"]"#, missing.0)),
            "{rendered}"
        );
    }

    #[test]
    fn history_get_reports_retention_eviction_without_losing_the_record() {
        let (_dir, mut store, _, second) = history_fixture();
        store
            .evict(&RetentionPolicy {
                max_image_bytes: 0,
                max_image_age: RetentionWindow::Forever,
            })
            .expect("evict images");
        let err = history_get(&mut store, &second.0, None, true).unwrap_err();
        assert_eq!(err.exit(), Exit::Storage);
        assert!(err.to_string().contains("evicted"), "{err}");
        assert!(store.record(&second).unwrap().is_some());
    }

    #[test]
    fn history_rows_include_source_metadata_and_a_human_badge() {
        let record = CaptureRecord {
            id: scrozz_store::CaptureId("capture-1".into()),
            created_at: scrozz_store::Timestamp(123),
            media_kind: MediaKind::Screenshot,
            pinned: true,
            source_app: SourceApp {
                name: Some("Safari".into()),
                identifier: Some("com.apple.Safari".into()),
                window_title: Some("Roadmap".into()),
            },
            window_shadow: Some(false),
            provenance: scrozz_core::Provenance::Window,
            target: CaptureTarget::Window(scrozz_core::WindowId("7".into())),
            frame: scrozz_store::FrameHeader {
                size: scrozz_core::PhysicalSize::new(1200.0, 800.0),
                stride: 4800,
                format: scrozz_core::PixelFormat::BgraPremultiplied8,
                color_space: scrozz_core::ColorSpace::DisplayP3,
                scale: scrozz_core::ScaleFactor::new(2.0),
            },
            image: ImageState::Present {
                hash: "hash".into(),
                byte_len: 4096,
            },
            ocr_text: None,
            annotation_count: 2,
        };

        let json = capture_record_json(&record).to_compact_string();
        assert!(json.contains(r#""name":"Safari""#), "{json}");
        assert!(
            json.contains(r#""identifier":"com.apple.Safari""#),
            "{json}"
        );
        assert!(json.contains(r#""window_shadow":false"#), "{json}");
        assert!(json.contains(r#""badge":"Safari""#), "{json}");
        let table = history_table(&[record], 1);
        assert!(table.contains("Safari / Roadmap"), "{table}");
        assert!(table.contains("yes"), "{table}");
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
        let rendered = with_settings("listing", || json_of(&["scrozz", "settings", "get"]));
        assert!(rendered.contains(r#""key":"capture.format""#), "{rendered}");
        assert!(
            rendered.contains(r#""key":"hotkey.record-stop""#),
            "{rendered}"
        );
    }

    #[test]
    fn reading_one_setting_returns_just_that_one() {
        let report = with_settings("read-one", || {
            run(&["scrozz", "settings", "get", "capture.quality"]).unwrap()
        });
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
        let err = with_settings("unknown", || {
            run(&["scrozz", "settings", "get", "capture.forrmat"]).unwrap_err()
        });
        assert_eq!(err.exit(), Exit::Usage);
        assert!(err.to_string().contains("capture.format"), "{err}");
    }

    #[test]
    fn a_bad_value_is_rejected_for_being_bad_not_for_being_unimplemented() {
        let err = with_settings("bad-value", || {
            run(&["scrozz", "settings", "set", "capture.format", "gif"]).unwrap_err()
        });
        assert_eq!(err.exit(), Exit::Usage);
        assert!(err.to_string().contains("png"), "{err}");
    }

    #[test]
    fn set_get_reset_and_path_share_one_persistent_document() {
        with_settings("round-trip", || {
            let set = run(&["scrozz", "settings", "set", "capture.format", "webp"]).unwrap();
            assert!(set.data.to_compact_string().contains(r#""source":"user""#));

            let get = run(&["scrozz", "settings", "get", "capture.format"]).unwrap();
            assert_eq!(get.human, "webp");

            let path = run(&["scrozz", "settings", "path"]).unwrap();
            assert!(path.human.ends_with("settings.json"), "{}", path.human);

            let reset = run(&["scrozz", "settings", "reset", "capture.format"]).unwrap();
            assert_eq!(reset.human, "capture.format = png");
            assert!(
                reset
                    .data
                    .to_compact_string()
                    .contains(r#""source":"default""#)
            );
        });
    }

    #[test]
    fn settings_path_still_works_when_the_document_needs_repair() {
        with_settings("corrupt-path", || {
            let path = settings_store::settings_path().unwrap();
            std::fs::write(&path, b"not json").unwrap();
            let report = run(&["scrozz", "settings", "path"]).unwrap();
            assert_eq!(report.human, path.to_string_lossy());
            let error = run(&["scrozz", "settings", "get"]).unwrap_err();
            assert_eq!(error.exit(), Exit::Storage);
        });
    }

    // -- URL automation and updates ---------------------------------------

    #[test]
    fn url_actions_are_inert_until_the_master_toggle_is_enabled() {
        with_settings("url-disabled", || {
            let err = run(&["scrozz", "url", "handle", "scrozz://capture/region"]).unwrap_err();
            assert_eq!(err.exit(), Exit::InvalidRequest);
            assert!(err.to_string().contains("disabled"), "{err}");
        });
    }

    #[test]
    fn enabled_url_automation_still_rejects_parameters_before_dispatch() {
        with_settings("url-parameters", || {
            run(&["scrozz", "url", "enable"]).unwrap();
            let err = run(&[
                "scrozz",
                "url",
                "handle",
                "scrozz://capture/region?output=/tmp/untrusted",
            ])
            .unwrap_err();
            assert_eq!(err.exit(), Exit::Usage);
            assert!(err.to_string().contains("not an allowed"), "{err}");
        });
    }

    #[test]
    fn every_url_action_maps_to_capture_or_record_only() {
        for action in [
            UrlAction::CaptureRegion,
            UrlAction::CaptureWindow,
            UrlAction::CaptureDisplay,
            UrlAction::CaptureAllDisplays,
            UrlAction::RecordRegion,
            UrlAction::RecordStop,
        ] {
            let command = command_for_url_action(action).unwrap();
            assert!(
                matches!(command, Command::Capture(_) | Command::Record(_)),
                "{action:?}"
            );
        }
    }

    #[test]
    fn update_status_is_persisted_but_has_no_release_key_yet() {
        with_settings("update-status", || {
            let report = run(&["scrozz", "update", "status"]).unwrap();
            let rendered = report.data.to_compact_string();
            assert!(rendered.contains(r#""phase":"idle""#), "{rendered}");
            assert!(rendered.contains(r#""trusted_keys":0"#), "{rendered}");
        });
    }

    #[test]
    fn update_check_is_gated_before_any_network_request() {
        with_settings("update-check", || {
            let err = run(&[
                "scrozz",
                "update",
                "check",
                "--manifest-url",
                "https://127.0.0.1/manifest.json",
                "--signature-url",
                "https://127.0.0.1/manifest.sig",
            ])
            .unwrap_err();
            assert_eq!(err.exit(), Exit::NotImplemented);
            assert!(err.to_string().contains("public key"), "{err}");
        });
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

    #[derive(Debug)]
    struct StubOcr;

    impl scrozz_ocr::Ocr for StubOcr {
        fn recognize(
            &self,
            _frame: &scrozz_core::Frame,
        ) -> scrozz_core::Result<Vec<scrozz_ocr::TextBlock>> {
            Ok(vec![
                scrozz_ocr::TextBlock {
                    text: "keep me".into(),
                    bounds: LogicalRect::new(
                        LogicalPoint::new(1.0, 1.0),
                        LogicalSize::new(6.0, 2.0),
                    ),
                    confidence: 0.95,
                },
                scrozz_ocr::TextBlock {
                    text: "discard me".into(),
                    bounds: LogicalRect::new(
                        LogicalPoint::new(1.0, 4.0),
                        LogicalSize::new(6.0, 2.0),
                    ),
                    confidence: 0.2,
                },
            ])
        }
    }

    #[test]
    fn ocr_on_a_stored_capture_filters_and_persists_searchable_text() {
        let (_dir, mut store, first, _) = history_fixture();
        let report = ocr_stored_capture(&mut store, &StubOcr, &first.0, Some(0.8))
            .expect("recognise stored capture");
        assert_eq!(report.human, "keep me");
        assert_eq!(
            store.record(&first).unwrap().unwrap().ocr_text.as_deref(),
            Some("keep me")
        );
        let found = store
            .search(&SearchQuery::all().text("keep me"))
            .expect("search OCR text");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, first);
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

    #[test]
    fn trusted_key_status_only_reports_the_release_gate_while_empty() {
        assert_eq!(trusted_update_keys_summary(0), "0 (release signing gated)");
        assert_eq!(trusted_update_keys_summary(2), "2");
    }
}
