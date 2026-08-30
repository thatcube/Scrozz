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

use std::path::Path;

use scrozz_annotate::{
    AnalysisCancellation, AutomaticBackground, Background, BackgroundImage, Beautification,
    BeautificationPreset, Document, Renderer as _, SkiaRenderer, analyze_smart_frame,
};
use scrozz_core::{
    Capture, CaptureRequest, CaptureTarget, CursorMode, Error as CoreError, Frame, PhysicalSize,
    Provenance, ScrollAxis, SelectionOptions, SelectionOutcome,
};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat, to_straight_rgba8};
use scrozz_ocr::{BarcodeDetector as _, Ocr as _};
use scrozz_record::{
    RecordingMachine, RecordingPhase, RecordingRequest, RecordingSettings,
    edit::{EditOutput, EditPlan, VideoDocument},
    transcode::{TranscodeCompletion, TranscodeOutput},
};
use scrozz_store::{
    CaptureId, CaptureRecord, DocumentState, History as _, ImageState, Page, SearchQuery,
    SqliteStore, Store, Timestamp,
};

use crate::{
    after_capture::{AfterCaptureSettings, current_availability},
    cli::{
        BarcodesArgs, BeautifyBackground, CaptureArgs, Command, Compositor, DisplaySelector,
        HistoryCommand, HotkeyCommand, InteractiveMode, ListWhat, OcrSubject, RecordArgs,
        SettingsCommand, ShareArgs, Sink, TargetSpec,
    },
    fault::{CliError, CliResult},
    gui::action::CaptureKind,
    gui::pipeline::capture_kind,
    gui::selection::CaptureSelector,
    hotkey_config, ipc,
    json::Json,
    platform,
    report::Report,
    settings,
    shortcuts::{ShortcutAction, ShortcutStore},
};

/// Somewhere for a dispatched capture's pixels to go, besides its own sinks.
///
/// A capture typed at a terminal while the menu-bar app is running executes
/// *inside* that app — that is what forwarding is for. Its pixels must
/// therefore be able to join the capture stack the user is already looking at,
/// and everything downstream of the stack: history identity, a bounded texture,
/// and Pin to Screen.
///
/// Without this seam the command wrote its file and the pixels ended there,
/// which quietly made pinning a fullscreen-only feature. Region and window
/// captures had a working backend and durable storage, and no route to a pin,
/// because the only producer that reached the card pipeline was the hotkey.
///
/// The capture is handed over **by value**: the running app moves the frame
/// into its own pipeline rather than keeping a second full-resolution copy
/// alive for the duration of the reply.
pub type CaptureSink<'a> = &'a mut dyn FnMut(CaptureKind, scrozz_core::Capture) -> CliResult<()>;

pub(crate) use crate::scrolling::{
    ScrollingTarget, fail_if_terminal_abort, resolve_scrolling_target, seal_terminal_output,
    wayland_portal_picker_target, wayland_scrolling_capture_target,
};

/// A sink whose bytes are already written but not yet visible.
///
/// Staging every destination before publishing any of them is what makes a
/// cancelled capture leave nothing behind: a Ctrl+C that arrives while the
/// second of three files is being written must not leave the first one on disk.
enum PreparedSink {
    File(crate::output::StagedFile),
    Clipboard,
    Stdout,
}

/// Runs a command locally.
///
/// # Errors
///
/// Whatever the command produces. Cancellation arrives here as
/// [`scrozz_core::Error::Cancelled`] and is rendered as an outcome, not a fault.
pub fn dispatch(command: &Command) -> CliResult<Report> {
    dispatch_inner(command, None, &mut |_, _| Ok(()), true)
}

/// Runs a command locally, offering any capture it takes to `observed`.
///
/// A run with nowhere to put pixels uses [`dispatch`]; the observer exists for
/// the one caller that has a live capture stack to put them in.
///
/// # Errors
///
/// The observer runs only after capture and requested sinks succeed. Its own
/// admission failure is propagated so an IPC caller is not acknowledged before
/// the running app has accepted the pixels.
pub fn dispatch_observed(command: &Command, observed: CaptureSink<'_>) -> CliResult<Report> {
    dispatch_inner(command, None, observed, false)
}

/// Runs a command with an existing-loop selector supplied by the GUI.
///
/// Forwarded interactive captures use this entry point from a worker thread, so
/// the synchronous selector contract can wait while the main eframe loop paints
/// and handles input. The observer is the same seam [`dispatch_observed`] uses,
/// so an interactively chosen target reaches the capture stack exactly as a
/// named one does.
///
/// # Errors
///
/// As [`dispatch_observed`].
pub fn dispatch_observed_with_selector(
    command: &Command,
    selector: &dyn CaptureSelector,
    observed: CaptureSink<'_>,
) -> CliResult<Report> {
    dispatch_inner(command, Some(selector), observed, false)
}

fn dispatch_inner(
    command: &Command,
    selector: Option<&dyn CaptureSelector>,
    observed: CaptureSink<'_>,
    sound_at_source: bool,
) -> CliResult<Report> {
    match command {
        Command::Capture(args) => capture(args, selector, observed, sound_at_source),
        Command::Record(args) => record(args, selector),
        Command::List(args) => list(args.what),
        Command::History(args) => history(&args.command),
        Command::Ocr(args) => ocr(args),
        Command::Share(args) => share(args),
        Command::Barcodes(args) => barcodes(args),
        Command::Settings(args) => settings_command(&args.command),
        Command::Hotkey(args) => hotkey(&args.command),
        Command::Gui => gui(),
    }
}

// ---------------------------------------------------------------------------
// share
// ---------------------------------------------------------------------------

fn share(args: &ShareArgs) -> CliResult<Report> {
    let shared = crate::cloud::share_file(args)?;
    let expiry = match shared.expires_seconds {
        Some(seconds) => Json::obj([
            ("mode", Json::str("provider-enforced")),
            ("seconds", Json::Int(i64::from(seconds))),
        ]),
        None => Json::obj([("mode", Json::str("public")), ("seconds", Json::Null)]),
    };
    let data = Json::obj([
        ("url", Json::str(shared.url.as_str())),
        ("key", Json::str(shared.key.as_str())),
        ("provider", Json::str(shared.provider)),
        ("encrypted", Json::Bool(shared.encrypted)),
        ("expiry", expiry),
        (
            "lifecycle_rule",
            Json::opt(shared.lifecycle_rule.as_deref(), Json::str),
        ),
    ]);
    let encryption = if shared.encrypted {
        " The uploaded object contains only client-side AES-256-GCM ciphertext."
    } else {
        ""
    };
    let expiry = match (shared.expires_seconds, &shared.lifecycle_rule) {
        (Some(seconds), Some(rule)) => format!(
            "\n\nThe provider enforces this link for {seconds} seconds. To delete the \
             object too, merge these rule fragment(s) into the bucket's existing lifecycle \
             configuration (do not replace its other rules):\n{rule}"
        ),
        (None, _) => "\n\nThis is a public/custom-domain URL; the bucket or CDN must already \
                      allow public GET requests."
            .to_owned(),
        _ => String::new(),
    };
    Ok(Report::new(
        data,
        format!("{}{}{}", shared.url, encryption, expiry),
    ))
}

// ---------------------------------------------------------------------------
// capture
// ---------------------------------------------------------------------------

/// The pixels a capture ultimately reports, however it got there.
///
/// Most captures never touch the annotation pipeline: `&capture.frame` is
/// returned untouched, exactly as before beautification existed, so the
/// overwhelming common case pays no rendering cost. Only a beautified capture
/// builds a [`Document`] and renders it, and the two paths must still produce
/// one `&Frame` the rest of `capture` can treat uniformly.
enum CapturedFrame {
    /// No beautification was requested; the source capture is reported as-is.
    Direct(Capture),
    /// Beautification ran; `rendered` is the framed output.
    ///
    /// `provenance` is captured separately because building the [`Document`]
    /// consumes the source [`Capture`] it came from.
    Rendered {
        provenance: Provenance,
        rendered: Frame,
    },
}

impl CapturedFrame {
    fn frame(&self) -> &Frame {
        match self {
            Self::Direct(capture) => &capture.frame,
            Self::Rendered { rendered, .. } => rendered,
        }
    }

    fn provenance(&self) -> Provenance {
        match self {
            Self::Direct(capture) => capture.provenance,
            Self::Rendered { provenance, .. } => *provenance,
        }
    }

    /// Yields the one owned capture this frame represents.
    ///
    /// Takes `self` so the full-resolution pixels are **moved** into whatever
    /// accepts them. A beautified capture consumed its source [`Capture`] while
    /// building the [`Document`], so its rendered frame is re-attached to the
    /// target and provenance it came from rather than cloned back out.
    fn into_capture(self, target: CaptureTarget) -> Capture {
        match self {
            Self::Direct(capture) => capture,
            Self::Rendered {
                provenance,
                rendered,
            } => Capture {
                frame: rendered,
                provenance,
                target,
            },
        }
    }
}

fn capture(
    args: &CaptureArgs,
    selector: Option<&dyn CaptureSelector>,
    observed: CaptureSink<'_>,
    sound_at_source: bool,
) -> CliResult<Report> {
    args.validate()?;
    // `target_spec`, not `target.resolve`: `--scrolling` is its own selector and
    // resolving only the ordinary target flags would silently take an
    // interactive region capture instead.
    let requested_target = args.target_spec()?;
    let scrolling_axis = match &requested_target {
        TargetSpec::Scrolling { axis, .. } => Some(*axis),
        _ => None,
    };
    let sinks = args.sinks();
    let selection = args.selection_options(None)?;
    let beautification = resolve_beautification(args)?;

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
        ("cursor", Json::Bool(args.cursor)),
        ("window_shadow", Json::Bool(!args.no_window_shadow)),
        ("format", Json::str(args.format().slug())),
        ("quality", Json::opt(args.quality, |q| Json::Int(q.into()))),
        ("delay_secs", Json::opt(args.delay, Json::Float)),
        (
            "beautification",
            Json::opt(beautification.as_ref(), beautification_json),
        ),
        ("smart_frame", Json::Bool(args.smart_frame)),
        ("sinks", Json::arr(sinks.iter().map(sink_json))),
    ]);

    if args.dry_run {
        return Ok(Report::new(
            Json::obj([("dry_run", Json::Bool(true)), ("plan", plan)]),
            describe_plan("Would capture", &requested_target, &sinks),
        ));
    }
    // Resolve ambient non-action preferences before reading pixels. A malformed
    // settings document must not turn a completed file/clipboard write into a
    // failure-shaped command after the side effect already happened.
    let (persisted, _) = settings::stored_settings()?;
    let screenshot_sound = settings::screenshot_sound(&persisted)?;

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
            let (outcome, frozen) = select_target(&options, args, selector, false)?;
            (outcome.target.clone(), Some(outcome), frozen)
        }
        concrete => (capture_target(&concrete)?, None, None),
    };

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
        std::thread::sleep(std::time::Duration::from_secs_f64(secs));
    }
    if selection_outcome.is_none()
        && let Some(selector) = selector
    {
        selector.begin_capture(backend.excludes_current_process(&request.target))?;
    }

    // Scrolling is the only acquisition that outlives a single call, so it is
    // also the only one that installs a terminal cancellation contract: one
    // Ctrl+C keeps what has already been stitched, two discard it.
    let (capture, mut terminal_cancel) = match scrolling_axis {
        Some(axis) => {
            let (capture, cancel) =
                crate::scrolling::scrolling_capture(backend.as_ref(), request.clone(), axis, None)?;
            (capture, Some(cancel))
        }
        None => {
            let capture = match frozen_capture {
                Some(capture) => capture,
                None => crate::gui::selection::capture_selected(
                    backend.as_ref(),
                    &request,
                    selection_outcome.as_ref(),
                )?,
            };
            (capture, None)
        }
    };
    lifecycle.finish();
    if let Some(outcome) = selection_outcome.as_ref() {
        remember_selection(outcome, backend.as_ref());
    }
    let frame_source = if args.requests_beautification() {
        let provenance = capture.provenance;
        let mut document = Document::new(capture);
        if args.smart_frame {
            let unframed = SkiaRenderer::new().render(&document)?;
            let mut smart =
                analyze_smart_frame(&unframed, provenance, &AnalysisCancellation::default())?
                    .beautification;
            apply_beautification_overrides(args, &mut smart)?;
            document.set_beautification(Some(smart))?;
        } else {
            document.set_beautification(beautification)?;
        }
        let rendered = SkiaRenderer::new().render(&document)?;
        CapturedFrame::Rendered {
            provenance,
            rendered,
        }
    } else {
        CapturedFrame::Direct(capture)
    };
    let frame = frame_source.frame();

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
            // D18: any folder the user picks, which is what lets a Dropbox or
            // iCloud directory provide sync for free with no service on our side.
            // Staged against the same settings snapshot this pass started with.
            Sink::DefaultFolder => prepared.push(PreparedSink::File(
                crate::output::StagedFile::for_settings(&bytes, &persisted)?,
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
                let clipboard_png = if args.format() == crate::cli::Format::Png {
                    bytes.clone()
                } else {
                    FrameEncoder::new()
                        .encode(frame, scrozz_export::ImageFormat::Png)
                        .map_err(CliError::Core)?
                };
                scrozz_shell::write_capture_to_clipboard(frame, &clipboard_png)
                    .map_err(CliError::Core)?;
                written.push("clipboard".to_string());
            }
            PreparedSink::Stdout => raw = Some(bytes.clone()),
        }
    }
    if sound_at_source && let Err(error) = scrozz_shell::play_screenshot_sound(&screenshot_sound) {
        tracing::warn!(%error, "the screenshot succeeded but its sound could not play");
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
        (
            "provenance",
            Json::str(format!("{:?}", frame_source.provenance())),
        ),
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

    // After the sinks, so a capture that could not be written never reaches the
    // stack claiming it was, and after the report is built, so the observer
    // cannot change what a script sees. The frame is *moved*: a forwarded
    // caller is not told the capture succeeded until the running app has taken
    // ownership of the pixels, and no second full-resolution copy is kept alive
    // while the reply is written.
    let card_kind = capture_kind(frame_source.provenance());
    observed(card_kind, frame_source.into_capture(request.target.clone()))?;

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
    surface_can_remain_visible: bool,
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
        let outcome = selector.select_for_capture(
            &capabilities.honour(options),
            cursor,
            surface_can_remain_visible,
        )?;
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

/// Resolves the beautification a plan would apply, for both `--dry-run`
/// reporting and the real capture path.
///
/// Returns `None` when nothing was requested, so a plain capture keeps
/// costing nothing beyond the encoder it always paid for. `--smart-frame`
/// still gets a value here — the automatic-background placeholder a dry run
/// shows — even though a real capture replaces it with the analysed result,
/// because a dry run has no pixels yet to analyse.
fn resolve_beautification(args: &CaptureArgs) -> CliResult<Option<Beautification>> {
    if !args.requests_beautification() {
        return Ok(None);
    }

    let mut beautification = if args.smart_frame {
        Beautification {
            auto_balance: true,
            background: Background::Automatic(AutomaticBackground::default()),
            ..Beautification::default()
        }
    } else {
        Beautification::preset(
            args.beautify
                .map_or(BeautificationPreset::Clean, |preset| preset.to_model()),
        )
    };
    apply_beautification_overrides(args, &mut beautification)?;
    Ok(Some(beautification))
}

/// Applies every beautification override the CLI accepted onto a starting
/// point, be it a named preset or a Smart Frame analysis.
fn apply_beautification_overrides(
    args: &CaptureArgs,
    beautification: &mut Beautification,
) -> CliResult<()> {
    if let Some(background) = &args.background {
        beautification.background = match background {
            BeautifyBackground::Transparent => Background::Transparent,
            BeautifyBackground::Solid(color) => Background::Solid(*color),
            BeautifyBackground::BuiltIn(background) => Background::BuiltIn(*background),
            BeautifyBackground::Image(path) => {
                let frame = scrozz_export::decode_file(path)?;
                let image = to_straight_rgba8(&frame)?;
                Background::Image(BackgroundImage::new(
                    image.width,
                    image.height,
                    image.data,
                    frame.color_space,
                )?)
            }
        };
    }
    if let Some(padding) = args.padding {
        beautification.padding = padding;
    }
    // `--frame-aspect` and `--size` both describe the output canvas and are
    // mutually exclusive on the CLI (`clap`'s `conflicts_with`), but the model
    // still needs whichever one was set to override the other, since a preset
    // or Smart Frame analysis may already have populated both fields.
    if let Some(aspect) = args.frame_aspect {
        beautification.aspect = aspect.to_model();
        beautification.output_size = None;
    }
    if let Some(size) = args.size {
        beautification.output_size = Some(size.to_model());
        beautification.aspect = scrozz_annotate::AspectPreset::Original;
    }
    if let Some(alignment) = args.alignment {
        beautification.alignment = alignment.to_model();
    }
    if args.auto_balance {
        beautification.auto_balance = true;
    }
    if let Some(radius) = args.corner_radius {
        beautification.corner_radius = radius;
    }
    if let Some(shadow) = args.shadow {
        beautification.shadow = shadow;
    }
    if let Some(border) = args.border {
        beautification.border_width = border;
    }
    beautification.validate()?;
    Ok(())
}

/// Renders a [`Beautification`] into the plan's JSON, in the same shape
/// whether it came from a preset, explicit overrides, or Smart Frame analysis.
fn beautification_json(beautification: &Beautification) -> Json {
    Json::obj([
        ("padding", Json::Float(beautification.padding)),
        (
            "aspect",
            Json::str(format!("{:?}", beautification.aspect).to_lowercase()),
        ),
        (
            "output_size",
            Json::opt(beautification.output_size, |size| {
                Json::obj([
                    ("width", Json::Int(i64::from(size.width))),
                    ("height", Json::Int(i64::from(size.height))),
                ])
            }),
        ),
        (
            "alignment",
            Json::str(format!("{:?}", beautification.alignment).to_lowercase()),
        ),
        ("auto_balance", Json::Bool(beautification.auto_balance)),
        ("corner_radius", Json::Float(beautification.corner_radius)),
        ("shadow", Json::Float(beautification.shadow)),
        ("border", Json::Float(beautification.border_width)),
        (
            "background",
            Json::str(match &beautification.background {
                Background::Transparent => "transparent".to_owned(),
                Background::Solid(color) => {
                    format!(
                        "#{:02x}{:02x}{:02x}{:02x}",
                        color.r, color.g, color.b, color.a
                    )
                }
                Background::Gradient { .. } => "gradient".to_owned(),
                Background::BuiltIn(background) => format!("{background:?}").to_lowercase(),
                Background::Automatic(background) => format!(
                    "automatic:v{}:#{:02x}{:02x}{:02x}-#{:02x}{:02x}{:02x}",
                    background.algorithm_version,
                    background.start.r,
                    background.start.g,
                    background.start.b,
                    background.end.r,
                    background.end.g,
                    background.end.b
                ),
                Background::Image(image) => {
                    format!("image:{}x{}", image.width(), image.height())
                }
                Background::Desktop(image) => {
                    format!("desktop:{}x{}", image.width(), image.height())
                }
                Background::BlurredSource { blur_radius, .. } => {
                    format!("blurred-source:{blur_radius}")
                }
            }),
        ),
    ])
}

// ---------------------------------------------------------------------------
// record
// ---------------------------------------------------------------------------

/// A resolved recording request, ready for either the CLI owner or the GUI
/// machine.
///
/// Both front ends build one of these and neither builds a
/// [`RecordingRequest`] itself, so the destination, encoder policy and reported
/// plan cannot drift apart between them.
pub(crate) struct PreparedRecording {
    pub(crate) request: RecordingRequest,
    pub(crate) destination: std::path::PathBuf,
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

fn record(args: &RecordArgs, selector: Option<&dyn CaptureSelector>) -> CliResult<Report> {
    if args.stop {
        // Reaching here means no instance was running, because a running one
        // would have handled it. There is no session in this process to stop.
        return Err(CliError::Core(CoreError::InvalidRequest(
            "no recording is in progress; `record --stop` talks to the running \
             Scrozz, and nothing is running"
                .to_string(),
        )));
    }

    let requested = recording_target_spec(args)?;
    let plan = recording_plan(args, &requested);

    if args.dry_run {
        return Ok(Report::new(
            Json::obj([("dry_run", Json::Bool(true)), ("plan", plan)]),
            format!(
                "Would record {} at {} fps.",
                describe_target(&requested),
                args.fps
            ),
        ));
    }

    let target = resolve_recording_capture_target(&requested, selector)?;
    let plan = recording_plan_for_target(args, &target);
    run_owned_recording(prepare_recording(args, target, plan)?)
}

/// Runs a recording this process owns, until another `scrozz record --stop`
/// arrives on the instance socket.
///
/// A recording has a beginning and an end, and the end has to come from
/// somewhere. With no application running, the only channel that exists is the
/// instance endpoint — which is free precisely because nothing is running. So
/// the recording owns it for its lifetime and answers exactly one command.
fn run_owned_recording(prepared: PreparedRecording) -> CliResult<Report> {
    if !cfg!(unix) {
        // Without a listener there is no channel a later `record --stop` could
        // arrive on, and a recording that cannot be stopped is worse than one
        // that never starts. Said plainly rather than started and abandoned.
        return Err(CliError::Core(CoreError::Unsupported {
            what: "starting a recording from the command line on this platform".to_owned(),
            why: "this build has no instance listener, so `record --stop` could never reach \
                  the recording; run the Scrozz application and record from there"
                .to_owned(),
        }));
    }
    let mut settings = RecordingSettings::shipped();
    let (persisted, _) = settings::stored_settings()?;
    settings.after_capture = persisted.recording_policy();

    let mut machine = RecordingMachine::native(settings).map_err(CliError::Core)?;
    let server = crate::gui::server::Server::bind()?;
    let fallback_target = prepared.request.target.clone();
    machine
        .begin_request(prepared.request)
        .map_err(CliError::Core)?;

    // Everything past this point must reach `stop_reply`, whether it succeeded
    // or not. A `?` here would drop an unanswered request, close its socket,
    // and leave the terminal that asked to stop the recording staring at an
    // empty response instead of the reason it could not be finalised.
    let mut tick = std::time::Instant::now();
    let mut stop_reply = None;
    let outcome = loop {
        if let Some(request) = server.poll()
            && let Some(reply) = owned_recording_stop(request)
        {
            stop_reply = Some(reply);
            break finalise_owned_recording(&mut machine);
        }
        let now = std::time::Instant::now();
        if let Err(error) = machine.tick(now.saturating_duration_since(tick)) {
            break Err(CliError::Core(error));
        }
        tick = now;
        for event in machine.drain_events().collect::<Vec<_>>() {
            if let scrozz_record::MachineEvent::Warning(message) = event {
                tracing::warn!("recording warning: {message}");
            }
        }
        if matches!(
            machine.phase(),
            RecordingPhase::Finished | RecordingPhase::Failed
        ) {
            break finalise_owned_recording(&mut machine);
        }
        std::thread::sleep(OWNED_RECORDING_POLL);
    };
    let report = outcome.and_then(|output| match output {
        Some(recording) => finish_recording_report(recording, Some(&fallback_target)),
        None => Err(machine.failure().map_or_else(
            || {
                CliError::Core(CoreError::Platform(
                    "the recording ended without producing output".to_owned(),
                ))
            },
            |failure| CliError::Core((*failure.error).clone()),
        )),
    });
    if let Some(reply) = stop_reply {
        reply.answer_and_wait_delivery(&report);
    }
    drop(server);
    report
}

/// Stops and drains an owned recording, returning whatever survived.
fn finalise_owned_recording(
    machine: &mut RecordingMachine,
) -> CliResult<Option<scrozz_record::Recording>> {
    if machine.is_active() {
        machine.stop().map_err(CliError::Core)?;
    }
    if machine.requires_finalisation() {
        let session = machine.begin_finalising().map_err(CliError::Core)?;
        let result = session.stop();
        machine
            .complete_finalising(result)
            .map_err(CliError::Core)?;
    }
    Ok(machine.output().cloned())
}

/// How often an owned CLI recording checks the socket and advances its clock.
const OWNED_RECORDING_POLL: std::time::Duration = std::time::Duration::from_millis(50);

fn owned_recording_stop(
    request: crate::gui::server::Request,
) -> Option<crate::gui::server::Request> {
    use clap::Parser as _;

    let mut argv = Vec::with_capacity(request.argv.len() + 1);
    argv.push("scrozz".to_owned());
    argv.extend(request.argv.iter().cloned());
    let stops = crate::cli::Cli::try_parse_from(argv)
        .ok()
        .and_then(|cli| cli.command)
        .is_some_and(|command| matches!(command, Command::Record(args) if args.stop));
    if stops {
        return Some(request);
    }
    // Anything else reaching an owned recording is served normally, so a
    // concurrent `scrozz list displays` is not silently dropped.
    request.serve_with(|_, _| Ok(()));
    None
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
    let destination = match &args.output {
        Some(path) => absolute_recording_path(path.clone())?,
        None => crate::output::default_recording_path()?,
    };
    let mut request = RecordingRequest::new(target);
    request.destination = Some(destination.clone());
    request.microphone = args.microphone;
    request.system_audio = args.system_audio;
    if let Some(device) = &args.camera {
        let settings = scrozz_record::settings::CameraSettings {
            enabled: true,
            position: scrozz_record::settings::OverlayAnchor::from_slug(&args.camera_position)?,
            size: f32::from(args.camera_size) / 100.0,
            shape: scrozz_record::settings::CameraShape::from_slug(&args.camera_shape)?,
            presenter: args.presenter,
            presenter_screen: !args.presenter_camera_only,
            mirror: !args.no_camera_mirror,
            border: !args.no_camera_border,
            shadow: !args.no_camera_shadow,
            ..scrozz_record::settings::CameraSettings::default()
        };
        settings.validate()?;
        let mut camera = scrozz_record::CameraRequest::new(settings);
        if device != "default" {
            camera = camera.with_device(scrozz_record::CameraDeviceId::new(device.clone())?);
        }
        request.camera = Some(camera);
    }
    request.fps = args.fps;
    request.show_cursor = args.cursor;
    request.validate().map_err(CliError::Core)?;
    Ok(PreparedRecording {
        request,
        destination,
        plan,
    })
}

/// The target a `record` invocation means, defaulting to the active display.
///
/// A bare `scrozz record` has to record *something*, and the display the
/// pointer is on is the only defensible guess. It is stated here rather than
/// inside the GUI so both front ends agree about what "no target" means.
pub(crate) fn recording_target_spec(args: &RecordArgs) -> CliResult<TargetSpec> {
    if args.target.is_unspecified() {
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
        ("microphone", Json::Bool(args.microphone)),
        ("system_audio", Json::Bool(args.system_audio)),
        (
            "camera",
            Json::opt(args.camera.as_deref(), |device| {
                Json::obj([
                    ("device", Json::str(device)),
                    ("presenter", Json::Bool(args.presenter)),
                    ("presenter_screen", Json::Bool(!args.presenter_camera_only)),
                    ("position", Json::str(&args.camera_position)),
                    ("size_percent", Json::Int(i64::from(args.camera_size))),
                    ("shape", Json::str(&args.camera_shape)),
                    ("mirror", Json::Bool(!args.no_camera_mirror)),
                    ("border", Json::Bool(!args.no_camera_border)),
                    ("shadow", Json::Bool(!args.no_camera_shadow)),
                ])
            }),
        ),
        ("cursor", Json::Bool(args.cursor)),
        ("output", Json::opt(args.output.as_deref(), path_json)),
    ])
}

/// Chooses a recording target on screen, remembering the region when asked.
pub(crate) fn select_recording_target_with_memory(
    mode: InteractiveMode,
    selector: Option<&dyn CaptureSelector>,
    remember_region: bool,
) -> CliResult<CaptureTarget> {
    let options = recording_selection_options(mode, remember_region)?;
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

fn resolve_recording_capture_target(
    target: &TargetSpec,
    selector: Option<&dyn CaptureSelector>,
) -> CliResult<CaptureTarget> {
    match target {
        TargetSpec::Interactive(mode) => select_recording_target_with_memory(*mode, selector, true),
        concrete => capture_target(concrete),
    }
}

fn absolute_recording_path(path: std::path::PathBuf) -> CliResult<std::path::PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()
        .map_err(|error| {
            CliError::Core(CoreError::Platform(format!(
                "could not resolve the recording destination: {error}"
            )))
        })?
        .join(path))
}

/// A finished recording's history identity and its report.
pub(crate) struct FinishedRecording {
    /// The typed history row, when one could be written.
    pub(crate) history_id: Option<CaptureId>,
    /// The report, or the failure the recording itself represents.
    pub(crate) report: CliResult<Report>,
}

/// Persists a finished recording and reports it.
///
/// History failure is recorded in the report rather than replacing it: the
/// video exists on disk either way, and telling the user their recording failed
/// because an index row could not be written would be a lie.
pub(crate) fn finish_recording(
    recording: &scrozz_record::Recording,
    fallback_target: Option<&CaptureTarget>,
) -> FinishedRecording {
    if let Err(error) = recording.require_native() {
        return FinishedRecording {
            history_id: None,
            report: Err(CliError::Core(error)),
        };
    }
    let (history_id, history_error) = match persist_recording(recording, fallback_target) {
        Ok(id) => (Some(id), None),
        Err(error) => {
            tracing::warn!("recording was saved but could not enter history: {error}");
            (None, Some(error.to_string()))
        }
    };
    let report = recording_report(recording, history_id.as_ref(), history_error.as_deref());
    FinishedRecording { history_id, report }
}

/// The report half of [`finish_recording`], for callers with no card to make.
pub(crate) fn finish_recording_report(
    recording: scrozz_record::Recording,
    fallback_target: Option<&CaptureTarget>,
) -> CliResult<Report> {
    finish_recording(&recording, fallback_target).report
}

/// Adds a finished recording to history as a typed video row.
///
/// The durable media file is referenced, never copied and never owned by
/// history: deleting the row removes the sidecar, the index entry and any
/// poster, and leaves the video exactly where the user can still find it.
pub(crate) fn persist_recording(
    recording: &scrozz_record::Recording,
    fallback_target: Option<&CaptureTarget>,
) -> CliResult<CaptureId> {
    recording.require_native().map_err(CliError::Core)?;
    let (engine, native_target) = match &recording.provenance {
        scrozz_record::RecordingProvenance::Native { engine, target } => {
            (engine.clone(), target.as_ref())
        }

        scrozz_record::RecordingProvenance::Synthetic { .. } => {
            return Err(CliError::Core(CoreError::InvalidRequest(
                "synthetic recording output never enters history".to_owned(),
            )));
        }
    };
    let target = native_target.or(fallback_target).cloned().ok_or_else(|| {
        CliError::Core(CoreError::Storage(
            "native recording did not report its capture target".to_owned(),
        ))
    })?;
    let path = std::fs::canonicalize(&recording.path).map_err(|error| {
        CliError::Core(CoreError::Storage(format!(
            "could not resolve the finished recording {}: {error}",
            recording.path.display()
        )))
    })?;
    // Every native summary field the recorder reported is carried across.
    // History is the only place a finished recording's encoder settings survive
    // once the process ends, so dropping any of them here would make them
    // unrecoverable rather than merely unshown.
    let video = scrozz_store::VideoMetadata {
        path,
        duration_secs: recording.duration_secs,
        engine,
        completion: video_completion(&recording.completion),
        size: recording.metadata.size,
        frames: recording.metadata.frames,
        audio_channels: recording.metadata.audio_channels,
        file_size_bytes: recording.metadata.file_size_bytes,
        // Slugs, because the recorder owns these vocabularies and history only
        // has to record faithfully what was used, not re-model it.
        codec: recording
            .metadata
            .video_codec
            .map(|codec| codec.slug().to_owned()),
        content_type: Some("video/mp4".to_owned()),
        quality: recording
            .metadata
            .quality
            .map(|quality| quality.slug().to_owned()),
        resolution: recording
            .metadata
            .resolution
            .map(scrozz_record::RecordingResolution::slug),
    };
    let mut store = platform::store()?;
    Ok(store.insert_recording(
        scrozz_store::NewRecording::new(video)
            .with_provenance(provenance_for_target(&target))
            .with_target(target),
    )?)
}

fn video_completion(
    completion: &scrozz_record::RecordingCompletion,
) -> scrozz_store::VideoCompletion {
    match completion {
        scrozz_record::RecordingCompletion::Complete => scrozz_store::VideoCompletion::Complete,
        scrozz_record::RecordingCompletion::Partial {
            salvageability,
            reason,
        } => scrozz_store::VideoCompletion::Partial {
            salvageability: match salvageability {
                scrozz_record::Salvageability::InitialisationOnly => {
                    scrozz_store::VideoSalvageability::InitialisationOnly
                }
                scrozz_record::Salvageability::Playable => {
                    scrozz_store::VideoSalvageability::Playable
                }
            },
            reason: reason.clone(),
        },
    }
}

/// Adds a finished editor export to history as a typed media row.
///
/// The exported file is durable and externally owned exactly as a recording is.
/// Everything the container decides — extension, media type and codec — comes
/// from the plan that produced it rather than from a guess, so a GIF enters
/// history as a GIF and a WebM enters it as `video/webm`.
pub(crate) fn persist_transcode_output(
    document: &VideoDocument,
    plan: &EditPlan,
    output: &TranscodeOutput,
) -> CliResult<CaptureId> {
    output.require_native().map_err(CliError::Core)?;
    let target = match &document.recording().provenance {
        scrozz_record::RecordingProvenance::Native {
            target: Some(target),
            ..
        } => target.clone(),
        scrozz_record::RecordingProvenance::Native { target: None, .. } => {
            return Err(CliError::Core(CoreError::Storage(
                "edited recording source did not retain its capture target".to_owned(),
            )));
        }
        scrozz_record::RecordingProvenance::Synthetic { .. } => {
            return Err(CliError::Core(CoreError::InvalidRequest(
                "synthetic media cannot enter capture history".to_owned(),
            )));
        }
    };
    let completion = match &output.completion {
        TranscodeCompletion::Complete => scrozz_store::VideoCompletion::Complete,
        TranscodeCompletion::Partial { reason } => scrozz_store::VideoCompletion::Partial {
            // A retained partial export always contains written frames; an
            // export that never produced one fails instead of being retained.
            salvageability: scrozz_store::VideoSalvageability::Playable,
            reason: reason.clone(),
        },
    };
    let path = std::fs::canonicalize(&output.path).map_err(|error| {
        CliError::Core(CoreError::Storage(format!(
            "could not resolve the completed export {}: {error}",
            output.path.display()
        )))
    })?;
    let (width, height) = plan.output_dimensions(document.metadata());
    let video = scrozz_store::VideoMetadata {
        path,
        duration_secs: plan.trim.duration().as_secs_f64(),
        engine: output.producer().to_owned(),
        completion,
        size: Some(PhysicalSize::new(f64::from(width), f64::from(height))),
        // A frame count is only knowable ahead of time for the formats whose
        // cadence the plan fixes; native video keeps the source cadence.
        frames: (!matches!(plan.output, EditOutput::Video))
            .then(|| plan.export_estimate(document.metadata()).frame_count),
        audio_channels: Some(plan.output_audio_channels(document.metadata())),
        file_size_bytes: Some(output.bytes_written),
        codec: Some(plan.output.codec_slug().to_owned()),
        content_type: Some(plan.output.media_type().to_owned()),
        quality: Some(plan.quality.slug().to_owned()),
        resolution: Some(plan.resolution.slug().to_owned()),
    };
    let provenance = provenance_for_target(&target);
    let mut store = platform::store()?;
    Ok(store.insert_recording(
        scrozz_store::NewRecording::new(video)
            .with_provenance(provenance)
            .with_target(target),
    )?)
}

const fn provenance_for_target(target: &CaptureTarget) -> Provenance {
    match target {
        CaptureTarget::Display(_) => Provenance::Display,
        CaptureTarget::Window(_) => Provenance::Window,
        CaptureTarget::Region(_) => Provenance::Region,
        CaptureTarget::AllDisplays => Provenance::AllDisplays,
    }
}

fn recording_report(
    recording: &scrozz_record::Recording,
    history_id: Option<&CaptureId>,
    history_error: Option<&str>,
) -> CliResult<Report> {
    let (completion, salvageability, reason) = match &recording.completion {
        scrozz_record::RecordingCompletion::Complete => ("complete", "playable", None),
        scrozz_record::RecordingCompletion::Partial {
            salvageability,
            reason,
        } => (
            "partial",
            match salvageability {
                scrozz_record::Salvageability::InitialisationOnly => "initialisation-only",
                scrozz_record::Salvageability::Playable => "playable",
            },
            Some(reason.as_str()),
        ),
    };
    let (engine, target) = match &recording.provenance {
        scrozz_record::RecordingProvenance::Native { engine, target } => {
            (engine.as_str(), target.as_ref())
        }
        scrozz_record::RecordingProvenance::Synthetic { generator } => {
            return Err(CliError::Core(CoreError::InvalidRequest(format!(
                "synthetic recording output from {generator} is not a real capture"
            ))));
        }
    };
    let data = Json::obj([
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
            "audio_channels",
            Json::opt(recording.metadata.audio_channels, |channels| {
                Json::Int(i64::from(channels))
            }),
        ),
        (
            "camera",
            Json::opt(recording.metadata.camera.as_deref(), |camera| {
                Json::obj([
                    ("presenter", Json::Bool(camera.presenter)),
                    ("presenter_screen", Json::Bool(camera.presenter_screen)),
                    ("shape", Json::str(camera.shape.slug())),
                    ("mirrored", Json::Bool(camera.mirrored)),
                    ("dropped_frames", Json::Int(camera.dropped_frames as i64)),
                ])
            }),
        ),
    ]);
    if let Some(reason) = reason {
        // Retained partial output is still a failed recording: the path is in
        // the message so the user can go and look at what survived.
        return Err(CliError::Core(CoreError::Platform(format!(
            "recording did not finish cleanly and a {salvageability} partial was retained at {}: {reason}",
            recording.path.display()
        ))));
    }
    Ok(Report::new(
        data,
        format!(
            "Recorded {:.2} seconds to {}.",
            recording.duration_secs,
            recording.path.display()
        ),
    ))
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
    match what {
        ListWhat::Displays => {
            let displays = platform::target_enumerator()?.displays()?;
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
            let windows = platform::target_enumerator()?.windows()?;
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
        ListWhat::Cameras => {
            let devices = scrozz_record::camera_devices()?;
            let data = Json::arr(devices.iter().map(|device| {
                Json::obj([
                    ("id", Json::str(device.id.as_str())),
                    ("name", Json::str(&device.name)),
                    (
                        "state",
                        Json::str(match device.state {
                            scrozz_record::CameraDeviceState::Available => "available",
                            scrozz_record::CameraDeviceState::Busy => "busy",
                            scrozz_record::CameraDeviceState::Disconnected => "disconnected",
                            scrozz_record::CameraDeviceState::PermissionDenied => {
                                "permission-denied"
                            }
                        }),
                    ),
                    ("default", Json::Bool(device.is_default)),
                ])
            }));
            let human = devices
                .iter()
                .map(|device| {
                    format!(
                        "{}  {}{}",
                        device.id.as_str(),
                        device.name,
                        if device.is_default { "  (default)" } else { "" }
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
        HistoryCommand::Pin { id, unpin } => set_history_pin(store, id, *unpin),
        HistoryCommand::UnlockPins => unlock_history_pins(store),
    }
}

fn set_history_pin(store: &mut impl Store, id: &str, unpin: bool) -> CliResult<Report> {
    let pinned = !unpin;
    store.set_pinned(&CaptureId(id.to_owned()), pinned)?;
    Ok(Report::new(
        Json::obj([
            ("id", Json::str(id)),
            ("pinned", Json::Bool(pinned)),
            ("screen_pin_cleared", Json::Bool(unpin)),
        ]),
        if pinned {
            format!("Pinned {id}.")
        } else {
            format!("Unpinned {id}.")
        },
    ))
}

fn unlock_history_pins(store: &mut scrozz_store::SqliteStore) -> CliResult<Report> {
    let unlocked = store.unlock_screen_pins()?;
    Ok(Report::new(
        Json::obj([(
            "unlocked",
            Json::Int(i64::try_from(unlocked).unwrap_or(i64::MAX)),
        )]),
        match unlocked {
            0 => "No pinned captures were locked.".into(),
            1 => "Unlocked 1 pinned capture.".into(),
            count => format!("Unlocked {count} pinned captures."),
        },
    ))
}

fn history_get(
    store: &mut SqliteStore,
    id: &str,
    output: Option<&Path>,
    stdout: bool,
) -> CliResult<Report> {
    let capture_id = CaptureId(id.to_owned());
    let document = match store.document(&capture_id)? {
        Some(DocumentState::Complete(document)) => *document,
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
        Some(crate::output::export_default(&bytes)?)
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
        ("app_name", Json::opt(record.app_name.as_deref(), Json::str)),
        (
            "window_title",
            Json::opt(record.window_title.as_deref(), Json::str),
        ),
        ("provenance", Json::str(provenance_slug(record.provenance))),
        (
            "width",
            Json::opt(record.frame.as_ref(), |frame| Json::Float(frame.size.width)),
        ),
        (
            "height",
            Json::opt(record.frame.as_ref(), |frame| {
                Json::Float(frame.size.height)
            }),
        ),
        (
            "scale",
            Json::opt(record.frame.as_ref(), |frame| {
                Json::Float(frame.scale.get())
            }),
        ),
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
        let app_title = match (&record.app_name, &record.window_title) {
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
    if !platform::ocr_available() {
        return Err(CliError::Core(CoreError::Unsupported {
            what: "recognising text".to_string(),
            why: "this build has no OCR engine. macOS uses Vision, packaged Windows uses \
                  Windows.Media.Ocr, and Linux plus portable Windows use the default \
                  `tesseract` subprocess integration without linking C libraries."
                .to_string(),
        }));
    }

    let engine = platform::ocr_engine(args.options());
    let engine_name = scrozz_ocr::SystemOcr::engine_name()?;
    if args.list_languages {
        let languages = engine.available_languages()?;
        let data = Json::obj([
            ("engine", Json::str(engine_name)),
            (
                "automatic_language_detection",
                Json::Bool(scrozz_ocr::SystemOcr::supports_automatic_language_detection()),
            ),
            (
                "languages",
                Json::arr(
                    languages
                        .iter()
                        .map(|language| Json::str(language.as_str())),
                ),
            ),
        ]);
        return Ok(Report::new(data, languages.join("\n")));
    }

    if args.under_pointer {
        let capture = platform::capture_backend()?;
        let display = capture.active_display()?;
        let captured = capture.capture(&CaptureRequest {
            target: CaptureTarget::Display(display.id),
            cursor: CursorMode::Hidden,
            include_window_shadow: false,
        })?;
        let pointer = scrozz_shell::pointer_location()?;
        let live = scrozz_ocr::LiveOcr::with_options(args.options());
        let mut blocks: Vec<_> = live
            .recognize_global_at(&captured.frame, display.bounds, pointer)?
            .into_iter()
            .collect();
        if let Some(minimum) = args.min_confidence {
            blocks.retain(|block| block.confidence >= minimum);
        }
        return Ok(ocr_report(&blocks, "pointer", args, engine_name));
    }

    let subject = args.resolve()?;
    match subject {
        OcrSubject::File(path) => {
            if !path.exists() {
                return Err(CliError::Core(CoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{} does not exist", path.display()),
                ))));
            }
            let frame = platform::decode_image_file(&path)?;
            let blocks = confident_blocks(engine.recognize(&frame)?, args.min_confidence);
            Ok(ocr_report(
                &blocks,
                &path.display().to_string(),
                args,
                engine_name,
            ))
        }
        OcrSubject::Capture(id) => {
            let mut store = platform::store()?;
            ocr_stored_capture(&mut store, &engine, &id, args, engine_name)
        }
    }
}

fn ocr_stored_capture(
    store: &mut SqliteStore,
    engine: &impl scrozz_ocr::Ocr,
    id: &str,
    args: &crate::cli::OcrArgs,
    engine_name: &str,
) -> CliResult<Report> {
    let capture_id = CaptureId(id.to_owned());
    let document = match store.document(&capture_id)? {
        Some(DocumentState::Complete(document)) => *document,
        Some(DocumentState::ImageEvicted(_)) => return Err(history_image_evicted(id)),
        None => return Err(history_not_found(id)),
    };
    let blocks = confident_blocks(
        engine.recognize(&document.source().frame)?,
        args.min_confidence,
    );
    // The stored index stays plain text: it is a search key, not a rendering,
    // and must not change shape with the caller's line-break preference.
    let text = scrozz_ocr::plain_text(&blocks);
    store.set_ocr_text(&capture_id, Some(&text))?;
    Ok(ocr_report(&blocks, id, args, engine_name))
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
fn ocr_report(
    blocks: &[scrozz_ocr::TextBlock],
    source: &str,
    args: &crate::cli::OcrArgs,
    engine_name: &str,
) -> Report {
    let text = scrozz_ocr::text(blocks, args.line_breaks.to_ocr());
    let links = if args.detect_links {
        scrozz_ocr::links(blocks)
    } else {
        Vec::new()
    };

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
        (
            "links",
            Json::arr(links.iter().map(|link| {
                Json::obj([
                    ("kind", Json::str(link.kind.token())),
                    ("text", Json::str(link.text.as_str())),
                    ("target", Json::str(link.target.as_str())),
                    (
                        "block",
                        Json::opt(link.block, |index| Json::Int(index as i64)),
                    ),
                    ("x", Json::Float(link.bounds.origin.x)),
                    ("y", Json::Float(link.bounds.origin.y)),
                    ("width", Json::Float(link.bounds.size.width)),
                    ("height", Json::Float(link.bounds.size.height)),
                ])
            })),
        ),
        ("engine", Json::str(engine_name)),
        (
            "accuracy",
            Json::str(if args.under_pointer {
                "fast"
            } else {
                args.accuracy.token()
            }),
        ),
        ("line_breaks", Json::str(args.line_breaks.token())),
        (
            "language_mode",
            Json::str(if args.auto_detect_language {
                "automatic"
            } else if args.language.is_empty() {
                "system"
            } else {
                "preferred"
            }),
        ),
        (
            "languages",
            Json::arr(
                args.language
                    .iter()
                    .map(|language| Json::str(language.as_str())),
            ),
        ),
        (
            "min_confidence",
            Json::opt(args.min_confidence, |value| Json::Float(f64::from(value))),
        ),
    ]);

    Report::new(data, text)
}

// ---------------------------------------------------------------------------
// barcodes
// ---------------------------------------------------------------------------

fn barcodes(args: &BarcodesArgs) -> CliResult<Report> {
    match args.resolve()? {
        OcrSubject::File(path) => {
            if !path.exists() {
                return Err(CliError::Core(CoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{} does not exist", path.display()),
                ))));
            }
            let frame = platform::decode_image_file(&path)?;
            let detector = platform::barcode_engine(args.options());
            let found = detector.detect(&frame)?;
            Ok(barcode_report(&found, &path.display().to_string()))
        }
        OcrSubject::Capture(_) => {
            let _store = platform::store()?;
            Err(CliError::not_implemented(
                "decoding barcodes in a stored capture",
                "scrozz-store",
            ))
        }
    }
}

fn barcode_report(barcodes: &[scrozz_ocr::Barcode], source: &str) -> Report {
    let data = Json::obj([
        ("source", Json::str(source)),
        ("count", Json::Int(barcodes.len() as i64)),
        (
            "barcodes",
            Json::arr(barcodes.iter().map(|barcode| {
                Json::obj([
                    ("payload", Json::str(barcode.payload.as_str())),
                    ("symbology", Json::str(barcode.symbology.token())),
                    ("confidence", Json::Float(f64::from(barcode.confidence))),
                    ("x", Json::Float(barcode.bounds.origin.x)),
                    ("y", Json::Float(barcode.bounds.origin.y)),
                    ("width", Json::Float(barcode.bounds.size.width)),
                    ("height", Json::Float(barcode.bounds.size.height)),
                    (
                        "corners",
                        Json::arr(barcode.corners.iter().map(|corner| {
                            Json::obj([("x", Json::Float(corner.x)), ("y", Json::Float(corner.y))])
                        })),
                    ),
                    (
                        "link",
                        Json::opt(barcode.link(), |link| {
                            Json::obj([
                                ("kind", Json::str(link.kind.token())),
                                ("target", Json::str(link.target)),
                            ])
                        }),
                    ),
                ])
            })),
        ),
        (
            "engine",
            Json::str(scrozz_ocr::SystemBarcodes::engine_name()),
        ),
    ]);
    let human = barcodes
        .iter()
        .map(|barcode| barcode.payload.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Report::new(data, human)
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

fn settings_command(command: &SettingsCommand) -> CliResult<Report> {
    let shortcuts = settings::stored_shortcuts();
    let (persisted, store) = settings::stored_settings()?;
    match command {
        SettingsCommand::Get { key: None } => Ok(Report::new(
            Json::obj([(
                "settings",
                settings::all_json_resolved(&shortcuts, &persisted),
            )]),
            settings::all_human_resolved(&shortcuts, &persisted),
        )),

        SettingsCommand::Get { key: Some(key) } => {
            let setting = settings::lookup(key)?;
            let (value, source) = settings::resolve(setting, &shortcuts, &persisted);
            let text = if value.is_empty() {
                "(unassigned)".to_owned()
            } else {
                value.clone()
            };
            Ok(Report::new(setting.to_json_valued(&value, source), text))
        }

        SettingsCommand::Set { key, value } => {
            let setting = settings::lookup(key)?;
            setting.validate(value)?;

            if let Some((media, action)) = AfterCaptureSettings::resolve_key(setting.key) {
                let enabled = value == "true";
                let availability = current_availability(media, action);
                if enabled && !availability.available {
                    return Err(CliError::usage(format!(
                        "{key} cannot be enabled: {}",
                        availability
                            .reason
                            .unwrap_or("this action is unavailable in this build")
                    )));
                }
                store
                    .update(store.inferred_profile(), |latest| {
                        latest.set(media, action, enabled);
                        Ok(())
                    })
                    .map_err(CliError::Core)?;
                return Ok(Report::new(
                    setting.to_json_valued(
                        value,
                        if value == setting.default {
                            "default"
                        } else {
                            "user"
                        },
                    ),
                    format!("{key} is now {value}"),
                ));
            }

            // Shortcuts are the one area with somewhere to put a value. Storing
            // them here rather than only in the GUI matters because the CLI is
            // how a dotfiles setup configures a machine it has never opened.
            let Some(action) = ShortcutAction::from_stored_key(setting.key) else {
                store
                    .update(store.inferred_profile(), |latest| {
                        latest.set_value(key, value);
                        Ok(())
                    })
                    .map_err(CliError::Core)?;
                return Ok(Report::new(
                    setting.to_json_valued(
                        value,
                        if value == setting.default {
                            "default"
                        } else {
                            "user"
                        },
                    ),
                    format!("{key} is now {value}"),
                ));
            };

            let mut updated = shortcuts.clone();
            updated.set(action, Some(value.as_str()));
            // Checked against the *other* rows after the assignment, so the
            // conflict reported is the one the user just created rather than one
            // they merely re-typed.
            if let Err(problem) = updated.check(action, value) {
                return Err(CliError::usage(format!(
                    "{key} cannot be {value:?}: {problem}"
                )));
            }

            let store = ShortcutStore::default_location().map_err(CliError::Core)?;
            store.save(&updated).map_err(CliError::Core)?;

            let stored = updated.get(action).unwrap_or_default().to_owned();
            let text = if stored.is_empty() {
                format!("{key} is now unassigned")
            } else {
                format!("{key} is now {stored}")
            };
            Ok(Report::new(
                Json::obj([
                    ("key", Json::str(setting.key)),
                    ("value", Json::str(&stored)),
                    ("source", Json::str("user")),
                    ("path", Json::str(store.path().display().to_string())),
                ]),
                text,
            ))
        }
        SettingsCommand::Reload => Ok(Report::new(
            Json::obj([("state", Json::str("reloaded"))]),
            "Settings reloaded.",
        )),
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
        ("crosshair_mode", Json::str(options.crosshair_mode.slug())),
        ("magnifier", Json::Bool(options.magnifier)),
        ("crosshair", Json::Bool(options.crosshair)),
        ("dimension_label", Json::str(options.dimension_label.slug())),
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use clap::Parser;
    use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};
    use scrozz_store::{
        MediaKind, NewCapture, RetentionPolicy, RetentionWindow, SqliteStore,
        test_support::{ScratchDir, id_at, sample_document, scratch_dir},
    };

    use super::*;
    use crate::{cli::Cli, exit::Exit, test_env};

    struct SettingsFixture {
        _environment: test_env::EnvGuard,
        root: std::path::PathBuf,
    }

    impl Drop for SettingsFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn isolated_settings(label: &str) -> SettingsFixture {
        let environment = test_env::lock();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "scrozz-command-settings-{label}-{}-{nonce}",
            std::process::id()
        ));
        test_env::set(
            crate::after_capture::SETTINGS_FILE_ENV,
            &root.join("settings.json").to_string_lossy(),
        );
        SettingsFixture {
            _environment: environment,
            root,
        }
    }

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

    #[test]
    fn a_scrolling_dry_run_reports_a_noninteractive_stitched_target() {
        let rendered = json_of(&["scrozz", "capture", "--scrolling=primary", "--dry-run"]);
        assert!(rendered.contains(r#""kind":"scrolling""#), "{rendered}");
        assert!(rendered.contains(r#""selector":"primary""#), "{rendered}");
        assert!(rendered.contains(r#""interactive":false"#), "{rendered}");
    }

    #[test]
    fn a_scrolling_dry_run_names_the_axis_it_would_grow_along() {
        let vertical = json_of(&["scrozz", "capture", "--scrolling", "--dry-run"]);
        assert!(vertical.contains(r#""axis":"vertical""#), "{vertical}");

        let horizontal = json_of(&[
            "scrozz",
            "capture",
            "--scrolling",
            "--scroll-axis",
            "horizontal",
            "--dry-run",
        ]);
        assert!(
            horizontal.contains(r#""axis":"horizontal""#),
            "{horizontal}"
        );
    }

    #[test]
    fn every_capture_provenance_maps_to_the_card_kind_that_decides_its_chrome() {
        // The pin's chrome policy is chosen from the card kind, and D9 forbids
        // synthetic chrome on a window capture. A window capture that arrived
        // as a "fullscreen" card would gain a shadow and a corner radius the
        // real window never had. The mapping reads provenance rather than the
        // requested target, because an interactive request does not know what
        // the user will pick.
        assert_eq!(capture_kind(Provenance::Window), CaptureKind::Window);
        assert_eq!(capture_kind(Provenance::Region), CaptureKind::Region);
        assert_eq!(capture_kind(Provenance::Stitched), CaptureKind::Region);
        assert_eq!(capture_kind(Provenance::Display), CaptureKind::Fullscreen);
        assert_eq!(
            capture_kind(Provenance::AllDisplays),
            CaptureKind::Fullscreen
        );
    }

    #[test]
    fn a_dry_run_hands_no_pixels_to_the_capture_stack() {
        // A dry run returns before the backend is even opened. Offering the
        // observer anything here would put a card on screen for a capture that
        // never happened.
        let cli = Cli::try_parse_from(["scrozz", "capture", "--dry-run", "--region", "0,0,4,4"])
            .expect("valid dry run");
        let command = cli.command.expect("a command");
        let mut seen = Vec::new();
        let report = dispatch_observed(&command, &mut |kind, _| {
            seen.push(kind);
            Ok(())
        })
        .expect("a dry run always succeeds");
        assert!(report.human.starts_with("Would capture"));
        assert!(seen.is_empty(), "a dry run took no pixels to hand over");
    }

    #[test]
    fn history_pin_updates_retention_and_unpin_clears_it() {
        let dir = scratch_dir("cli-history-pin");
        let mut store = SqliteStore::open_ephemeral(dir.path()).expect("store");
        let id = store
            .insert(NewCapture::new(&sample_document(8, 8, 3, 0)))
            .expect("insert");

        let pinned = set_history_pin(&mut store, &id.0, false).expect("pin");
        assert_eq!(pinned.human, format!("Pinned {}.", id.0));
        assert!(store.record(&id).expect("record").expect("present").pinned);

        let unpinned = set_history_pin(&mut store, &id.0, true).expect("unpin");
        assert_eq!(unpinned.human, format!("Unpinned {}.", id.0));
        assert!(!store.record(&id).expect("record").expect("present").pinned);
        assert!(
            unpinned
                .data
                .to_compact_string()
                .contains(r#""screen_pin_cleared":true"#)
        );
    }

    #[test]
    fn history_unlock_pins_unlocks_every_pin_without_ids() {
        let dir = scratch_dir("cli-history-unlock-pins");
        let mut store = SqliteStore::open_ephemeral(dir.path()).expect("store");
        let mut ids = Vec::new();
        for seed in [21, 22] {
            let id = store
                .insert(NewCapture::new(&sample_document(8, 8, seed, 0)))
                .expect("insert");
            let mut state = scrozz_core::PinState::new(
                scrozz_core::LogicalRect::new(
                    scrozz_core::LogicalPoint::new(10.0, 20.0),
                    scrozz_core::LogicalSize::new(320.0, 180.0),
                ),
                scrozz_core::PinScale::ORIGINAL,
                None,
            );
            state.locked = true;
            store.set_screen_pin(&id, Some(&state)).expect("lock pin");
            ids.push(id);
        }

        let report = unlock_history_pins(&mut store).expect("unlock");
        assert_eq!(report.human, "Unlocked 2 pinned captures.");
        for id in ids {
            let record = store.record(&id).expect("read").expect("record");
            assert!(
                !record.screen_pin.expect("screen pin retained").locked,
                "unlocking must retain the pin and only clear its lock"
            );
        }
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

    #[test]
    fn ocr_json_shape_and_key_order_are_stable() {
        let cli = Cli::try_parse_from([
            "scrozz",
            "ocr",
            "fixture.png",
            "--min-confidence",
            "0.5",
            "--detect-links",
        ])
        .unwrap();
        let Some(Command::Ocr(args)) = cli.command else {
            panic!("expected OCR command")
        };
        let blocks = [scrozz_ocr::TextBlock {
            text: "https://example.org".to_string(),
            bounds: LogicalRect::new(LogicalPoint::new(1.0, 2.0), LogicalSize::new(3.0, 4.0)),
            confidence: 0.5,
        }];
        let suffix = format!(
            r#""engine":"{}","accuracy":"accurate","line_breaks":"preserve","language_mode":"system","languages":[],"min_confidence":0.5}}"#,
            scrozz_ocr::SystemOcr::engine_name().unwrap()
        );
        let expected = [
            r#"{"source":"fixture.png","block_count":1,"text":"https://example.org","#,
            r#""blocks":[{"text":"https://example.org","confidence":0.5,"x":1.0,"y":2.0,"width":3.0,"height":4.0}],"#,
            r#""links":[{"kind":"url","text":"https://example.org","target":"https://example.org","block":0,"x":1.0,"y":2.0,"width":3.0,"height":4.0}],"#,
            &suffix,
        ]
        .concat();

        assert_eq!(
            ocr_report(
                &blocks,
                "fixture.png",
                &args,
                scrozz_ocr::SystemOcr::engine_name().unwrap(),
            )
            .data
            .to_compact_string(),
            expected
        );
    }

    #[test]
    fn barcode_json_shape_and_key_order_are_stable() {
        let barcodes = [scrozz_ocr::Barcode {
            payload: "mailto:person@example.org".to_string(),
            symbology: scrozz_ocr::Symbology::QrCode,
            bounds: LogicalRect::new(LogicalPoint::new(1.0, 2.0), LogicalSize::new(3.0, 4.0)),
            corners: vec![
                LogicalPoint::new(1.0, 2.0),
                LogicalPoint::new(4.0, 2.0),
                LogicalPoint::new(4.0, 6.0),
                LogicalPoint::new(1.0, 6.0),
            ],
            confidence: 1.0,
        }];
        let suffix = format!(
            r#""link":{{"kind":"email","target":"mailto:person@example.org"}}}}],"engine":"{}"}}"#,
            scrozz_ocr::SystemBarcodes::engine_name()
        );
        let expected = [
            r#"{"source":"fixture.png","count":1,"barcodes":[{"payload":"mailto:person@example.org","symbology":"qr","confidence":1.0,"#,
            r#""x":1.0,"y":2.0,"width":3.0,"height":4.0,"corners":[{"x":1.0,"y":2.0},{"x":4.0,"y":2.0},{"x":4.0,"y":6.0},{"x":1.0,"y":6.0}],"#,
            &suffix,
        ]
        .concat();

        assert_eq!(
            barcode_report(&barcodes, "fixture.png")
                .data
                .to_compact_string(),
            expected
        );
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
    fn a_dry_run_reports_the_resolved_beautification_plan() {
        let rendered = json_of(&[
            "scrozz",
            "capture",
            "--region",
            "0,0,300,200",
            "--beautify",
            "social",
            "--background",
            "#11223380",
            "--frame-aspect",
            "wide",
            "--alignment",
            "bottom-right",
            "--border",
            "2",
            "--dry-run",
        ]);
        assert!(rendered.contains(r#""auto_balance":true"#), "{rendered}");
        assert!(rendered.contains(r#""aspect":"wide""#), "{rendered}");
        assert!(
            rendered.contains(r#""alignment":"bottomright""#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r##""background":"#11223380""##),
            "{rendered}"
        );
        assert!(rendered.contains(r#""border":2.0"#), "{rendered}");
    }

    #[test]
    fn a_dry_run_without_beautification_reports_no_beautification_plan() {
        let rendered = json_of(&["scrozz", "capture", "--region", "0,0,300,200", "--dry-run"]);
        assert!(rendered.contains(r#""beautification":null"#), "{rendered}");
        assert!(rendered.contains(r#""smart_frame":false"#), "{rendered}");
    }

    #[test]
    fn a_dry_run_reports_an_exact_output_size_and_clears_the_aspect() {
        let rendered = json_of(&[
            "scrozz",
            "capture",
            "--region",
            "0,0,300,200",
            "--beautify",
            "clean",
            "--size",
            "640x480",
            "--dry-run",
        ]);
        assert!(
            rendered.contains(r#""output_size":{"width":640,"height":480}"#),
            "{rendered}"
        );
    }

    #[test]
    fn a_dry_run_reports_smart_frame_opt_in_with_an_automatic_background() {
        let rendered = json_of(&[
            "scrozz",
            "capture",
            "--region",
            "0,0,300,200",
            "--smart-frame",
            "--dry-run",
        ]);
        assert!(rendered.contains(r#""smart_frame":true"#), "{rendered}");
        assert!(rendered.contains(r#""auto_balance":true"#), "{rendered}");
        assert!(
            rendered.contains(r#""background":"automatic:"#),
            "{rendered}"
        );
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
    fn a_camera_dry_run_reports_composition_without_opening_hardware() {
        let rendered = json_of(&[
            "scrozz",
            "record",
            "--dry-run",
            "--camera",
            "stable-device",
            "--presenter",
            "--camera-position",
            "top-left",
            "--camera-size",
            "32",
            "--camera-shape",
            "square",
        ]);
        assert!(
            rendered.contains(r#""device":"stable-device""#),
            "{rendered}"
        );
        assert!(rendered.contains(r#""presenter":true"#), "{rendered}");
        assert!(rendered.contains(r#""position":"top-left""#), "{rendered}");
        assert!(rendered.contains(r#""size_percent":32"#), "{rendered}");
        assert!(rendered.contains(r#""shape":"square""#), "{rendered}");
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
                "window_title",
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
    fn launching_the_gui_from_a_test_does_nothing_visible() {
        // Load-bearing: the test suite must never put a window on screen.
        let err = run(&["scrozz", "gui"]).unwrap_err();
        assert_eq!(err.exit(), Exit::NotImplemented);
    }

    // -- settings ----------------------------------------------------------

    #[test]
    fn listing_settings_works_today() {
        let _settings = isolated_settings("list");
        let rendered = json_of(&["scrozz", "settings", "get"]);
        assert!(rendered.contains(r#""key":"capture.format""#), "{rendered}");
        assert!(
            rendered.contains(r#""key":"hotkey.record-stop""#),
            "{rendered}"
        );
    }

    #[test]
    fn reading_one_setting_returns_just_that_one() {
        let _settings = isolated_settings("read");
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
        let _settings = isolated_settings("unknown");
        let err = run(&["scrozz", "settings", "get", "capture.forrmat"]).unwrap_err();
        assert_eq!(err.exit(), Exit::Usage);
        assert!(err.to_string().contains("capture.format"), "{err}");
    }

    #[test]
    fn a_bad_value_is_rejected_for_being_bad_not_for_being_unimplemented() {
        let _settings = isolated_settings("bad-value");
        // The distinction that matters: the user must learn about their mistake
        // before persistence is touched.
        let err = run(&["scrozz", "settings", "set", "capture.format", "gif"]).unwrap_err();
        assert_eq!(err.exit(), Exit::Usage);
        assert!(err.to_string().contains("png"), "{err}");
    }

    #[test]
    fn a_good_value_persists_across_reads() {
        let _settings = isolated_settings("round-trip");
        run(&["scrozz", "settings", "set", "capture.format", "webp"]).unwrap();
        let read = run(&["scrozz", "settings", "get", "capture.format"]).unwrap();
        assert_eq!(read.human, "webp");
        assert!(read.data.to_compact_string().contains(r#""source":"user""#));
    }

    #[test]
    fn an_unavailable_after_capture_action_cannot_be_enabled_from_the_cli() {
        let _settings = isolated_settings("unavailable");
        let err = run(&["scrozz", "settings", "set", "capture.pin-to-screen", "true"]).unwrap_err();
        assert_eq!(err.exit(), Exit::Usage);
        assert!(err.to_string().contains("not implemented"), "{err}");
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
        // Derived rather than literal: the region default is platform-specific,
        // so spelling it out here would assert the authoring machine.
        let accelerator = crate::hotkey_config::Accelerator::parse(
            crate::cli::HotkeyAction::CaptureRegion.default_accelerator(),
        )
        .unwrap()
        .to_sway();
        assert!(
            report
                .human
                .contains(&format!("bindsym {accelerator} exec scrozz capture"))
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
        let accelerator = crate::hotkey_config::Accelerator::parse(
            crate::cli::HotkeyAction::CaptureRegion.default_accelerator(),
        )
        .unwrap();
        let report = run(&[
            "scrozz",
            "hotkey",
            "generate-config",
            "--compositor",
            "hyprland",
        ])
        .unwrap();
        assert!(report.human.contains(&format!(
            "bind = {}, {}, exec, scrozz capture",
            accelerator.to_hyprland_modifiers(),
            accelerator.key
        )));
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
        let message = err.to_string();
        assert!(message.contains("no OCR engine"), "{message}");
        assert!(message.contains("tesseract"), "{message}");
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
        let cli = Cli::try_parse_from(["scrozz", "ocr", "--min-confidence", "0.8", &first.0])
            .expect("valid OCR arguments");
        let Some(Command::Ocr(args)) = cli.command else {
            panic!("expected OCR command")
        };
        let report = ocr_stored_capture(&mut store, &StubOcr, &first.0, &args, "stub")
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
}
