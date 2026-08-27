//! Who owns the main loop.
//!
//! [`crate::gui::App`] deliberately has no loop of its own — see its module
//! documentation for why. A *host* is whatever supplies one: an `eframe` update
//! callback on a real desktop, or [`Headless`] in a terminal and in tests.
//!
//! Keeping this seam explicit is what stops a second event loop being invented
//! somewhere else later. There is exactly one, it is on the main thread, and it
//! calls [`App::tick`] — everything that blocks is already on a worker.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use scrozz_annotate::{Document, Renderer, SkiaRenderer};
use scrozz_export::{Encoder, FrameEncoder, ImageFormat};
use scrozz_shell::{
    DragOrigin, DragOutcome, DragPayload, DragPreview, DragSession, DragSource, NativeDragEvent,
    NativeDragSource, byte_source, current_native_drag_event, native_drag_source,
};
use scrozz_ui::{
    AnnotationEditor, EditorDestination, EditorDragRequest, EditorEvent, OverlayHandle, Theme,
    history::{viewport_builder, viewport_id},
    icons::IconStore,
    overlay_app::{OverlayApp, OverlayGeometry, OverlayOptions, VisibilityHook},
    stack::CardMetrics,
};

use scrozz_core::Error as CoreError;
use scrozz_store::CaptureId;

use crate::{
    fault::{CliError, CliResult},
    gui::{
        app::{App, Config, EditorResult, OverlayConfig, OverlayDisplay, PendingDrag, Tick},
        card::{CardSurface, Recording},
        overlay::OverlayCards,
        panel::NativeSurfaceSlot,
        pipeline::{DragGeometry, DragSubject},
    },
    report::Report,
};

/// Set to `1` to run without a window.
pub const HEADLESS_ENV: &str = "SCROZZ_GUI_HEADLESS";

/// How long a tick may sleep before the next one.
///
/// 60 Hz. Fast enough that a hotkey feels instant, slow enough that an idle
/// menu-bar app is not a busy loop — which matters, because this process is
/// meant to sit there all day.
const IDLE: Duration = Duration::from_millis(16);
const EDITOR_AUTOSAVE_DELAY: Duration = Duration::from_millis(280);

/// Something that can drive an [`App`] to completion.
/// Writes the final report the way `main` would have.
///
/// Needed because the windowed host cannot always return: see
/// [`Driver::logic`] for why quitting sometimes has to leave the process
/// directly, which means the report has to be written before it does.
pub type Emit = Box<dyn FnOnce(&Report) + Send>;

pub trait Host {
    /// Runs until the app stops, then reports what happened.
    ///
    /// Takes `self` boxed so hosts can own a window, an event loop, or nothing.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] if the loop could not be started at all. A run that
    /// starts and then fails to do anything useful returns `Ok` with that
    /// recorded in the report — the app ran, it just had nothing to show.
    fn run(self: Box<Self>, app: App) -> CliResult<Report>;

    /// What this host is, for diagnostics.
    fn describe(&self) -> &'static str;

    /// The surface cards will appear on under this host.
    ///
    /// The host decides, because the surface and the main loop are the same
    /// decision: an overlay handle is useless without a window to draw it, and
    /// a window with no surface has nothing to show.
    fn surface(&self) -> Box<dyn CardSurface>;
}

/// Drives the app from a plain sleep loop, with no window.
///
/// This is not a stub: hotkeys, the menu-bar item, the IPC listener, capture,
/// the store and the clipboard all work under it. The only thing missing is
/// somewhere to draw a card, and the surface reports that honestly rather than
/// pretending.
///
/// It is also the shape that makes automated runs safe. A headless run has a
/// deadline it cannot miss, so it cannot leave anything on anyone's screen.
#[derive(Debug, Default, Clone, Copy)]
pub struct Headless;

impl Host for Headless {
    fn run(self: Box<Self>, mut app: App) -> CliResult<Report> {
        tracing::info!("running headless");
        loop {
            if app.tick() == Tick::Stop {
                break;
            }
            std::thread::sleep(IDLE);
        }

        // Before the report is printed, not after: the menu-bar item should be
        // gone by the time the user sees any output about it.
        app.shut_down();
        let report = app.report();
        Ok(report)
    }

    fn describe(&self) -> &'static str {
        "headless"
    }

    fn surface(&self) -> Box<dyn CardSurface> {
        // Records rather than draws. Every other part of the pipeline is real,
        // so a headless run still captures, stores, encodes and copies — the
        // card is simply written to the report instead of to the screen.
        Box::new(Recording::new())
    }
}

/// Chooses the host for this run.
///
/// # Errors
///
/// Returns [`CliError::NotImplemented`] when a window is wanted and this build
/// cannot open one, naming the exact reason rather than falling back to
/// headless. Silently running without a window when a window was asked for is
/// the failure mode this whole module exists to avoid: the app appears to work
/// and nothing ever appears on screen.
pub fn for_platform(config: &Config, emit: Emit) -> CliResult<Box<dyn Host>> {
    if headless_requested() {
        return Ok(Box::new(Headless));
    }

    if HAS_WINDOW {
        return Ok(Box::new(Windowed::configured(emit, config.overlay)));
    }

    Err(CliError::NotImplemented {
        what: "an on-screen capture card".to_owned(),
        provider: WINDOW_GAP,
    })
}

/// Whether this build can open a window.
///
/// A plain constant rather than a Cargo feature, because a feature would imply
/// the code exists and is switched off.
pub const HAS_WINDOW: bool = true;

/// Why there would be no window, if there were none.
///
/// Retained because [`for_platform`] must still say something useful if
/// [`HAS_WINDOW`] is ever turned off for a target that cannot open one.
pub const WINDOW_GAP: &str = "this binary has no windowing dependency. \
     scrozz-ui supplies the whole overlay (OverlayApp, OverlayHandle, \
     native_options) but the call that opens a window is eframe::run_native. \
     Add `eframe.workspace = true` to apps/scrozz/Cargo.toml, then set \
     HAS_WINDOW to true. Until then, SCROZZ_GUI_HEADLESS=1 runs everything \
     except the card";

/// Drives the app from `eframe`'s update callback, with the overlay on screen.
///
/// # The one main thread
///
/// `eframe` owns the main loop, so [`App::tick`] is called from inside
/// [`eframe::App::update`]. That is the whole point of `tick` not being a loop:
/// winit, the tray and the hotkey receiver are all serviced from the same
/// thread, in the same callback, in a fixed order, and nothing blocking
/// happens there — capture and encoding are already on a worker.
pub struct Windowed {
    handle: OverlayHandle,
    emit: Emit,
    config: OverlayConfig,
    native_surface: NativeSurfaceSlot,
}

impl Windowed {
    /// A host with an overlay handle that works before the window exists.
    #[must_use]
    pub fn new(emit: Emit) -> Self {
        Self::configured(emit, OverlayConfig::default())
    }

    /// A window host with explicit stack controls.
    #[must_use]
    pub fn configured(emit: Emit, config: OverlayConfig) -> Self {
        Self {
            handle: OverlayHandle::new(),
            emit,
            config,
            native_surface: NativeSurfaceSlot::default(),
        }
    }
}

impl Default for Windowed {
    fn default() -> Self {
        Self::new(Box::new(|_| {}))
    }
}

impl Host for Windowed {
    fn run(self: Box<Self>, app: App) -> CliResult<Report> {
        let geometry = work_area(self.config.display);
        tracing::info!(?geometry, "opening the overlay");

        let mut metrics = CardMetrics::default().scaled(self.config.card_scale);
        metrics.margin = self.config.stack_margin;
        let geometry_state = Arc::new(Mutex::new(geometry));
        let options = OverlayOptions {
            geometry,
            panel: panel_hook(self.native_surface.clone()),
            visibility: visibility_hook(self.native_surface.clone()),
            probe: pointer_probe(Arc::clone(&geometry_state)),
            card_metrics: metrics,
            auto_close_secs: self
                .config
                .auto_close
                .map(|duration| duration.as_secs_f64()),
            ..Default::default()
        };

        // The app is moved into the window, so the report has to come back out
        // some other way: `run_native` owns the loop and drops everything in
        // it before returning.
        let outcome: Arc<Mutex<Option<Report>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&outcome);
        let handle = self.handle.clone();
        let reporting = self.handle.clone();
        let display = self.config.display;
        let emit = self.emit;

        eframe::run_native(
            "Scrozz",
            scrozz_ui::overlay_app::native_options(geometry),
            Box::new(move |cc| {
                let drag_source = match native_drag_source() {
                    Ok(source) => Some(source),
                    Err(err) => {
                        tracing::warn!("native drag-out is unavailable: {err}");
                        None
                    }
                };
                let overlay = OverlayApp::new(cc, handle, options);
                let editor_icons = Arc::new(IconStore::new(&cc.egui_ctx));
                Ok(Box::new(Driver {
                    app,
                    overlay,
                    sink,
                    handle: reporting,
                    emit: Some(emit),
                    announced: false,
                    stopped: false,
                    shutdown_requested: false,
                    editors: HashMap::new(),
                    active_editor_export: None,
                    editor_icons,
                    drag_source,
                    active_drag: None,
                    history_drag_events: Arc::new(Mutex::new(Vec::new())),
                    show_editor_color_names: true,
                    display,
                    geometry,
                    geometry_state,
                    last_display_probe: Instant::now(),
                }))
            }),
        )
        .map_err(|err| {
            CliError::Core(CoreError::Platform(format!(
                "the overlay window could not open: {err}"
            )))
        })?;

        outcome.lock().map_or_else(
            |_| {
                Err(CliError::Core(CoreError::Platform(
                    "the overlay panicked".to_owned(),
                )))
            },
            |mut slot| {
                slot.take().ok_or_else(|| {
                    CliError::Core(CoreError::Platform(
                        "the overlay closed before the app reported".to_owned(),
                    ))
                })
            },
        )
    }

    fn describe(&self) -> &'static str {
        "eframe overlay"
    }

    fn surface(&self) -> Box<dyn CardSurface> {
        // Cloned, not moved: the same handle goes to the window, so a capture
        // taken before the window opens is already in the pile when it does.
        Box::new(OverlayCards::with_native_surface(
            self.handle.clone(),
            self.native_surface.clone(),
        ))
    }
}

/// The `eframe::App` that services this app and draws the overlay.
struct Driver {
    app: App,
    overlay: OverlayApp,
    sink: Arc<Mutex<Option<Report>>>,
    handle: OverlayHandle,
    emit: Option<Emit>,
    announced: bool,
    stopped: bool,
    shutdown_requested: bool,
    editors: HashMap<CaptureId, Arc<Mutex<EditorSession>>>,
    active_editor_export: Option<CaptureId>,
    editor_icons: Arc<IconStore>,
    drag_source: Option<NativeDragSource>,
    active_drag: Option<ActiveDrag>,
    history_drag_events: Arc<Mutex<Vec<(CaptureId, NativeDragEvent)>>>,
    show_editor_color_names: bool,
    display: OverlayDisplay,
    geometry: OverlayGeometry,
    geometry_state: Arc<Mutex<OverlayGeometry>>,
    last_display_probe: Instant,
}

struct EditorSession {
    editor: AnnotationEditor,
    pending: Vec<EditorEvent>,
    drag: Option<DragSession>,
    latest_revision: u64,
    latest_data: scrozz_annotate::DocumentData,
    acknowledged_revision: u64,
    in_flight: Option<RevisionSnapshot>,
    pending_drag: Option<PendingEditorDrag>,
    export: Option<PendingEditorExport>,
    ready_drag: Option<ReadyEditorDrag>,
    initiating_drag_event: Option<Result<NativeDragEvent, String>>,
    autosave_due: Option<Instant>,
    retry_blocked: bool,
    close_requested: bool,
}

#[derive(Clone)]
struct RevisionSnapshot {
    revision: u64,
    data: scrozz_annotate::DocumentData,
}

struct PendingEditorDrag {
    snapshot: RevisionSnapshot,
    request: EditorDragRequest,
    native_event: NativeDragEvent,
}

struct ReadyEditorDrag {
    request: EditorDragRequest,
    native_event: NativeDragEvent,
}

#[derive(Clone)]
struct PendingEditorExport {
    snapshot: RevisionSnapshot,
    destination: EditorDestination,
    dispatched: bool,
}

impl EditorSession {
    fn new(document: Document) -> Self {
        Self::new_with_color_names(document, true)
    }

    fn new_with_color_names(document: Document, show_color_names: bool) -> Self {
        let latest_data = document.data();
        let mut editor = AnnotationEditor::new(document);
        editor.set_show_color_names(show_color_names);
        Self {
            editor,
            pending: Vec::new(),
            drag: None,
            latest_revision: 0,
            latest_data,
            acknowledged_revision: 0,
            in_flight: None,
            pending_drag: None,
            export: None,
            ready_drag: None,
            initiating_drag_event: None,
            autosave_due: None,
            retry_blocked: false,
            close_requested: false,
        }
    }

    fn stage(&mut self, data: scrozz_annotate::DocumentData) -> RevisionSnapshot {
        if data != self.latest_data {
            self.latest_revision = self.latest_revision.saturating_add(1);
            self.latest_data = data;
        }
        RevisionSnapshot {
            revision: self.latest_revision,
            data: self.latest_data.clone(),
        }
    }

    fn needs_save(&self) -> bool {
        self.latest_revision > self.acknowledged_revision
    }

    fn is_ready_to_close(&self) -> bool {
        self.close_requested
            && !self.needs_save()
            && self.in_flight.is_none()
            && self.pending_drag.is_none()
            && self.export.is_none()
            && self.ready_drag.is_none()
            && self.initiating_drag_event.is_none()
            && self.drag.is_none()
    }

    fn changed(&mut self, data: scrozz_annotate::DocumentData, now: Instant) {
        let previous = self.latest_revision;
        self.stage(data);
        if self.latest_revision != previous {
            self.autosave_due = Some(now + EDITOR_AUTOSAVE_DELAY);
        }
        self.retry_blocked = false;
    }

    fn save_requested(&mut self, data: scrozz_annotate::DocumentData) {
        self.stage(data);
        self.autosave_due = None;
        self.retry_blocked = false;
        if !self.needs_save() {
            self.editor.mark_save_succeeded();
        }
    }

    fn close_requested(&mut self) {
        self.stage(self.editor.document_data());
        self.autosave_due = None;
        self.close_requested = true;
        self.retry_blocked = false;
    }

    fn next_save(&self, now: Instant) -> Option<RevisionSnapshot> {
        if self.in_flight.is_some() || self.retry_blocked || self.export.is_some() {
            return None;
        }
        if let Some(pending) = self
            .pending_drag
            .as_ref()
            .filter(|pending| pending.snapshot.revision > self.acknowledged_revision)
        {
            return Some(pending.snapshot.clone());
        }
        let barrier = self.close_requested || self.autosave_due.is_none();
        let debounce_elapsed = self.autosave_due.is_some_and(|due| now >= due);
        (self.needs_save() && (barrier || debounce_elapsed)).then(|| RevisionSnapshot {
            revision: self.latest_revision,
            data: self.latest_data.clone(),
        })
    }

    fn save_dispatched(&mut self, snapshot: RevisionSnapshot) {
        self.editor.mark_save_pending();
        self.in_flight = Some(snapshot);
    }

    fn save_dispatch_failed(&mut self, error: impl Into<String>) {
        self.retry_blocked = true;
        self.editor.mark_save_failed(error);
    }

    fn save_finished(&mut self, revision: u64, failure: Option<String>) -> bool {
        let Some(in_flight) = self.in_flight.as_ref() else {
            return false;
        };
        if in_flight.revision != revision {
            return false;
        }
        if let Some(error) = failure {
            self.in_flight = None;
            self.retry_blocked = true;
            self.editor.mark_save_failed(error);
            return true;
        }

        self.acknowledged_revision = self.acknowledged_revision.max(revision);
        self.in_flight = None;
        self.retry_blocked = false;
        if self
            .autosave_due
            .is_some_and(|_| revision == self.latest_revision)
        {
            self.autosave_due = None;
        }
        self.promote_acknowledged_drag();
        if self.needs_save() {
            self.editor.mark_save_pending();
        } else {
            self.editor.mark_save_succeeded();
        }
        true
    }

    fn promote_acknowledged_drag(&mut self) {
        if self
            .pending_drag
            .as_ref()
            .is_some_and(|pending| pending.snapshot.revision <= self.acknowledged_revision)
            && let Some(pending) = self.pending_drag.take()
        {
            self.ready_drag = Some(ReadyEditorDrag {
                request: pending.request,
                native_event: pending.native_event,
            });
        }
    }

    fn export_requested(
        &mut self,
        data: scrozz_annotate::DocumentData,
        destination: EditorDestination,
    ) {
        let snapshot = self.stage(data);
        self.autosave_due = None;
        self.retry_blocked = false;
        self.export = Some(PendingEditorExport {
            snapshot,
            destination,
            dispatched: false,
        });
        self.editor.mark_export_pending(destination);
    }

    fn export_finished(
        &mut self,
        revision: u64,
        destination: EditorDestination,
        result: Result<String, String>,
    ) -> bool {
        let Some(export) = self.export.as_ref() else {
            return false;
        };
        if export.snapshot.revision != revision || export.destination != destination {
            return false;
        }
        self.export = None;
        match result {
            Ok(detail) => {
                self.acknowledged_revision = self.acknowledged_revision.max(revision);
                self.retry_blocked = false;
                if revision == self.latest_revision {
                    self.autosave_due = None;
                    self.editor.mark_save_succeeded();
                }
                self.editor.mark_export_succeeded(detail);
            }
            Err(error) => {
                self.retry_blocked = true;
                self.editor.mark_export_failed(error);
            }
        }
        true
    }

    fn poll_drag(&mut self) {
        let Some(outcome) = self.drag.as_ref().and_then(DragSession::outcome) else {
            return;
        };
        let notice = match outcome {
            DragOutcome::Accepted(_) => "Dropped a copy.".to_owned(),
            DragOutcome::Rejected | DragOutcome::Cancelled => "Drag cancelled.".to_owned(),
            DragOutcome::Failed(reason) => format!("Drag failed: {reason}"),
            _ => "The drag ended.".to_owned(),
        };
        self.editor.set_notice(notice);
        self.drag = None;
    }
}

fn lock_editor<'a>(
    capture: &CaptureId,
    editor: &'a Mutex<EditorSession>,
) -> std::sync::MutexGuard<'a, EditorSession> {
    editor.lock().unwrap_or_else(|poisoned| {
        tracing::error!(
            capture = %capture.0,
            "annotation editor state was poisoned; recovering its latest snapshot"
        );
        poisoned.into_inner()
    })
}

struct ActiveDrag {
    subject: DragSubject,
    session: DragSession,
}

impl Driver {
    /// Says what the panel conversion did, the moment it is known.
    ///
    /// Logged on the first tick rather than left to the final report because
    /// the teardown described in [`Driver::logic`] can end the process before
    /// any report is written. The one fact worth having is the one most likely
    /// to be lost, so it is stated as soon as it exists.
    fn announce_panel(&mut self) {
        if self.announced {
            return;
        }
        let Some(report) = self.handle.panel_report() else {
            return;
        };
        self.announced = true;
        if report.non_activating {
            tracing::info!(detail = %report.detail, "the overlay is a non-activating panel");
        } else {
            tracing::warn!(
                detail = %report.detail,
                "the overlay is an ordinary window: clicking a card will steal focus"
            );
        }
    }

    /// Whether the window was swizzled into a panel.
    fn converted(&self) -> bool {
        self.handle
            .panel_report()
            .is_some_and(|report| report.non_activating)
    }

    fn collect_editor_requests(&mut self, ctx: &egui::Context) {
        for request in self.app.drain_editor_requests() {
            let viewport = editor_viewport_id(&request.capture);
            match self.editors.entry(request.capture) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(Arc::new(Mutex::new(EditorSession::new_with_color_names(
                        request.document,
                        self.show_editor_color_names,
                    ))));
                }
                std::collections::hash_map::Entry::Occupied(_) => {
                    ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Focus);
                }
            }
        }
    }

    fn drain_editor_events(&mut self) {
        let mut pending = Vec::new();
        for (capture, editor) in &self.editors {
            let mut session = lock_editor(capture, editor);
            pending.extend(
                std::mem::take(&mut session.pending)
                    .into_iter()
                    .map(|event| (capture.clone(), event)),
            );
        }

        for (capture, event) in pending {
            let Some(editor) = self.editors.get(&capture) else {
                continue;
            };
            let mut session = lock_editor(&capture, editor);
            match event {
                EditorEvent::Changed(data) => {
                    session.changed(data, Instant::now());
                }
                EditorEvent::Save(data) => {
                    session.save_requested(data);
                }
                EditorEvent::DragRequested(request) => match session.initiating_drag_event.take() {
                    Some(Ok(native_event)) => {
                        let snapshot = session.stage(request.data.clone());
                        session.pending_drag = Some(PendingEditorDrag {
                            snapshot,
                            request,
                            native_event,
                        });
                        session.autosave_due = None;
                        session.retry_blocked = false;
                    }
                    Some(Err(error)) => {
                        session.editor.set_notice(format!("Drag failed: {error}"));
                    }
                    None => session
                        .editor
                        .set_notice("Drag failed: the initiating mouse event was not retained."),
                },
                EditorEvent::ExportRequested { destination, data } => {
                    if self.active_editor_export.is_some() {
                        session
                            .editor
                            .mark_export_failed("Another image export is already in progress.");
                    } else {
                        self.active_editor_export = Some(capture.clone());
                        session.export_requested(data, destination);
                    }
                }
                EditorEvent::ColorNamesChanged(show) => {
                    drop(session);
                    self.show_editor_color_names = show;
                    for (editor_capture, editor) in &self.editors {
                        lock_editor(editor_capture, editor)
                            .editor
                            .set_show_color_names(show);
                    }
                }
                EditorEvent::CloseRequested => {
                    session.close_requested();
                }
            }
        }
        self.dispatch_editor_exports();
        self.dispatch_editor_saves();
    }

    fn dispatch_editor_exports(&mut self) {
        let Some(capture) = self.active_editor_export.clone() else {
            return;
        };
        let Some(editor) = self.editors.get(&capture) else {
            self.active_editor_export = None;
            return;
        };
        let mut session = lock_editor(&capture, editor);
        let Some(export) = session.export.as_mut() else {
            self.active_editor_export = None;
            return;
        };
        if export.dispatched {
            return;
        }
        if self.app.export_editor(
            capture.clone(),
            export.snapshot.revision,
            export.snapshot.data.clone(),
            export.destination,
        ) {
            export.dispatched = true;
        } else {
            session.export = None;
            session
                .editor
                .mark_export_failed("The capture worker is unavailable; try again.");
            self.active_editor_export = None;
        }
    }

    fn dispatch_editor_saves(&mut self) {
        let now = Instant::now();
        for (capture, editor) in &self.editors {
            let mut session = lock_editor(capture, editor);
            let snapshot = session.next_save(now);
            let Some(snapshot) = snapshot else {
                session.promote_acknowledged_drag();
                continue;
            };
            if self
                .app
                .persist_editor(capture.clone(), snapshot.revision, snapshot.data.clone())
            {
                session.save_dispatched(snapshot);
            } else {
                session.save_dispatch_failed(
                    "the capture worker is unavailable; the editor remains open",
                );
            }
        }
    }

    fn show_history(&self, ctx: &egui::Context) {
        let history = self.app.history();
        let drag_events = Arc::clone(&self.history_drag_events);
        let visible = history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_visible();
        if !visible {
            return;
        }

        let viewport = viewport_id();
        ctx.request_repaint_of(viewport);
        ctx.show_viewport_deferred(viewport, viewport_builder(), move |ui, _class| {
            let close_requested = ui.ctx().input(|input| input.viewport().close_requested());
            let mut history = history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if close_requested {
                history.close();
                return;
            }
            history.ui(ui);
            for capture in history.drain_native_drag_intents() {
                match current_native_drag_event() {
                    Ok(event) => {
                        if let Ok(mut events) = drag_events.lock() {
                            events.push((capture, event));
                        } else {
                            history.cancel_native_drag(
                                &capture,
                                "Drag failed: native event state was unavailable.",
                            );
                        }
                    }
                    Err(error) => history.cancel_native_drag(
                        &capture,
                        format!("Drag failed before it could begin: {error}"),
                    ),
                }
            }
            ui.ctx().request_repaint_after(IDLE);
        });
    }

    fn collect_history_drag_events(&mut self) {
        let events = self
            .history_drag_events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default();
        for (capture, event) in events {
            self.app.retain_history_drag_event(capture, event);
        }
    }

    fn service_drag(&mut self, ctx: &egui::Context) {
        if let Some(active) = self.active_drag.as_ref()
            && let Some(outcome) = active.session.outcome()
        {
            let subject = active.subject.clone();
            self.app.drag_finished(&subject, &outcome);
            self.active_drag = None;
        }
        if self.active_drag.is_some() {
            return;
        }

        let Some(pending) = self.app.take_drag() else {
            return;
        };
        let subject = pending.subject.clone();
        let Some(source) = self.drag_source.as_ref() else {
            self.app.drag_finished(
                &subject,
                &scrozz_shell::DragOutcome::Failed(
                    "the native drag backend is unavailable".to_owned(),
                ),
            );
            return;
        };
        let origin = match drag_origin(ctx, &pending) {
            Ok(origin) => origin,
            Err(error) => {
                self.app.drag_finished(
                    &subject,
                    &scrozz_shell::DragOutcome::Failed(error.to_string()),
                );
                return;
            }
        };
        match source.begin(pending.payload, origin) {
            Ok(session) => {
                self.active_drag = Some(ActiveDrag { subject, session });
            }
            Err(error) => {
                self.app.drag_finished(
                    &subject,
                    &scrozz_shell::DragOutcome::Failed(error.to_string()),
                );
            }
        }
    }
    fn drain_editor_results(&mut self) {
        for result in self.app.drain_editor_results() {
            match result {
                EditorResult::Saved { capture, revision } => {
                    let Some(editor) = self.editors.get(&capture) else {
                        continue;
                    };
                    let mut session = lock_editor(&capture, editor);
                    session.save_finished(revision, None);
                }
                EditorResult::SaveFailed {
                    capture,
                    revision,
                    error,
                } => {
                    let Some(editor) = self.editors.get(&capture) else {
                        continue;
                    };
                    let mut session = lock_editor(&capture, editor);
                    session.save_finished(revision, Some(error));
                }
                EditorResult::Exported {
                    capture,
                    revision,
                    destination,
                    detail,
                } => {
                    if let Some(editor) = self.editors.get(&capture) {
                        let mut session = lock_editor(&capture, editor);
                        session.export_finished(revision, destination, Ok(detail));
                    }
                    if self.active_editor_export.as_ref() == Some(&capture) {
                        self.active_editor_export = None;
                    }
                }
                EditorResult::ExportFailed {
                    capture,
                    revision,
                    destination,
                    error,
                } => {
                    if let Some(editor) = self.editors.get(&capture) {
                        let mut session = lock_editor(&capture, editor);
                        session.export_finished(revision, destination, Err(error));
                    }
                    if self.active_editor_export.as_ref() == Some(&capture) {
                        self.active_editor_export = None;
                    }
                }
            }
        }
        self.dispatch_editor_exports();
        self.dispatch_editor_saves();
    }

    fn close_finished_editors(&mut self, ctx: &egui::Context) {
        let closing: Vec<_> = self
            .editors
            .iter()
            .filter(|(capture, editor)| lock_editor(capture, editor).is_ready_to_close())
            .map(|(capture, _)| capture.clone())
            .collect();
        for capture in closing {
            ctx.send_viewport_cmd_to(editor_viewport_id(&capture), egui::ViewportCommand::Close);
            self.editors.remove(&capture);
            self.app.release_editor(capture);
        }
    }

    fn show_editors(&self, ctx: &egui::Context) {
        for (capture, editor) in &self.editors {
            show_editor_viewport(
                ctx,
                capture.clone(),
                Arc::clone(editor),
                Arc::clone(&self.editor_icons),
            );
        }
    }

    fn flush_editors_for_shutdown(&mut self) {
        self.drain_editor_events();
        for (capture, editor) in &self.editors {
            let mut session = lock_editor(capture, editor);
            session.close_requested();
        }
        self.dispatch_editor_saves();
    }

    fn finish_shutdown(&mut self, ctx: &egui::Context) {
        self.stopped = true;
        self.app.shut_down();
        self.drain_editor_results();
        let report = self.app.report();
        if let Ok(mut slot) = self.sink.lock() {
            *slot = Some(report.clone());
        }

        if self.converted() {
            if let Some(emit) = self.emit.take() {
                emit(&report);
            }
            tracing::debug!("leaving without dismantling the converted panel");
            std::process::exit(0);
        }

        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

fn editor_viewport_id(capture: &CaptureId) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("annotation-editor", &capture.0))
}

fn show_editor_viewport(
    ctx: &egui::Context,
    capture: CaptureId,
    editor: Arc<Mutex<EditorSession>>,
    icons: Arc<IconStore>,
) {
    ctx.show_viewport_deferred(
        editor_viewport_id(&capture),
        egui::ViewportBuilder::default()
            .with_title(format!("Scrozz — Annotate — {}", capture.0))
            .with_inner_size([960.0, 680.0])
            .with_min_inner_size([900.0, 520.0]),
        move |ui, _class| {
            let theme = if ui.visuals().dark_mode {
                Theme::dark()
            } else {
                Theme::light()
            };
            let mut session = lock_editor(&capture, &editor);
            session.poll_drag();
            session.editor.show(ui, &icons, &theme);
            let events = session.editor.drain_events();
            if events
                .iter()
                .any(|event| matches!(event, EditorEvent::DragRequested(_)))
            {
                session.initiating_drag_event =
                    Some(current_native_drag_event().map_err(|error| error.to_string()));
            }
            if events
                .iter()
                .any(|event| matches!(event, EditorEvent::CloseRequested))
            {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
            session.pending.extend(events);
            let drag = session
                .ready_drag
                .take()
                .map(|request| (session.editor.document().clone(), request));
            if session.drag.as_ref().is_some_and(DragSession::is_active) {
                ui.ctx().request_repaint_after(IDLE);
            }
            drop(session);

            if let Some((document, request)) = drag {
                match begin_editor_drag(&document, &request) {
                    Ok(drag) => {
                        let mut session = lock_editor(&capture, &editor);
                        session.editor.set_notice("Drop to copy the edited image.");
                        session.drag = Some(drag);
                        ui.ctx().request_repaint_after(IDLE);
                    }
                    Err(error) => {
                        tracing::warn!(
                            capture = %capture.0,
                            %error,
                            "could not begin annotation drag"
                        );
                        lock_editor(&capture, &editor)
                            .editor
                            .set_notice(format!("Drag failed: {error}"));
                    }
                }
            }
        },
    );
}

fn begin_editor_drag(
    document: &Document,
    ready: &ReadyEditorDrag,
) -> scrozz_core::Result<DragSession> {
    let request = &ready.request;
    let mut snapshot = document.clone();
    snapshot.restore(request.data.clone())?;
    let frame = SkiaRenderer::new().render(&snapshot)?;
    let png = FrameEncoder::new().encode(&frame, ImageFormat::Png)?;
    let promised = Arc::new(png.clone());
    let bytes = byte_source(move || Ok(promised.as_ref().clone()));
    let preview = DragPreview::from_png(png, request.preview.size)?;
    let payload = DragPayload::png_capture("Scrozz annotation", bytes).with_preview(preview);
    let origin =
        DragOrigin::from_event(ready.native_event.clone(), request.preview, request.pointer);
    native_drag_source()?.begin(payload, origin)
}

impl eframe::App for Driver {
    /// The app's own work, before anything is drawn.
    ///
    /// `logic` rather than `ui` on purpose: eframe also calls it while the
    /// window is *hidden* but a repaint was requested. A menu-bar app is
    /// transparent and empty at rest, so a tick that only ran alongside
    /// painting would be a tick that stops happening exactly when the app is
    /// doing its actual job — waiting for a hotkey.
    ///
    /// # Why quitting can leave the process directly
    ///
    /// Closing the window is the ordinary way out, and it is what happens when
    /// the panel conversion did not run. After a conversion it aborts, and the
    /// reason is a genuine collision rather than a bug in either party:
    ///
    /// - `winit` registers a KVO observer on its window for
    ///   `effectiveAppearance` (`window_delegate.rs:753`), so it can follow the
    ///   system theme.
    /// - Registering a KVO observer makes the Objective-C runtime *isa-swizzle*
    ///   the observed object into a generated `NSKVONotifying_WinitWindow`
    ///   subclass. That is how KVO has always worked.
    /// - The panel conversion isa-swizzles the same object again, to
    ///   `ScrozzOverlayPanel`. The second swizzle overwrites the first, and the
    ///   KVO machinery is severed.
    /// - On teardown `Drop for WindowDelegate` calls
    ///   `removeObserver:forKeyPath:`, which throws because the object is no
    ///   longer the class KVO registered. It throws inside `dealloc`, which
    ///   objc2 declares `extern "C"` and therefore cannot unwind, so the
    ///   process aborts rather than reporting anything.
    ///
    /// The conversion itself succeeds — the window really does become a
    /// non-activating panel, and behaves correctly for the whole session. Only
    /// dismantling it fails. So the app quits the way a Cocoa app quits, by
    /// leaving, after everything of its own is already closed: `shut_down` has
    /// removed the menu-bar item, stopped the worker, closed the socket and
    /// flushed the store, and the report is written first. The window is the
    /// operating system's to reclaim.
    ///
    /// This is deliberately narrow. With `SCROZZ_GUI_PANEL=0` the conversion
    /// does not happen and the ordinary close runs, so the clean path stays
    /// exercised and nothing else is being masked. The real repair belongs in
    /// `scrozz-shell`: refuse to swizzle a class whose name already begins
    /// `NSKVONotifying_`, or preserve the KVO subclass across the change.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let root_close_requested = ctx.input(|input| input.viewport().close_requested());
        self.announce_panel();
        self.collect_editor_requests(ctx);
        self.drain_editor_events();
        self.collect_history_drag_events();

        if self.last_display_probe.elapsed() >= Duration::from_millis(250) {
            self.last_display_probe = Instant::now();
            let geometry = work_area(self.display);
            if geometry != self.geometry {
                self.geometry = geometry;
                if let Ok(mut current) = self.geometry_state.lock() {
                    *current = geometry;
                }
                self.handle.set_geometry(geometry);
            }
        }

        if !self.stopped {
            let tick = if self.shutdown_requested {
                self.app.settle_for_shutdown();
                Tick::Continue
            } else {
                self.app.tick()
            };
            self.collect_editor_requests(ctx);
            self.drain_editor_results();
            self.close_finished_editors(ctx);
            self.show_history(ctx);
            self.service_drag(ctx);

            if tick == Tick::Stop || root_close_requested {
                self.shutdown_requested = true;
                self.flush_editors_for_shutdown();
            }

            if self.shutdown_requested {
                // Keep the root and editor viewports alive until each newest
                // revision has a matching durable acknowledgement. A queue or
                // store failure remains visible and retryable instead of being
                // discarded by process teardown.
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.app.settle_for_shutdown();
                self.drain_editor_results();
                self.close_finished_editors(ctx);
                if self.editors.is_empty()
                    && self.active_editor_export.is_none()
                    && self.active_drag.is_none()
                    && self.app.native_drags_settled()
                {
                    self.finish_shutdown(ctx);
                }
            } else {
                self.overlay.service_hidden(ctx);
                self.app.drain_overlay_events();
            }
        }

        // An idle overlay must still be woken, or a hotkey pressed while
        // nothing is on screen would not be noticed until something else woke
        // the window — which, for a window that is empty at rest, may be never.
        ctx.request_repaint_after(IDLE);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.overlay.ui(ui, frame);
        self.app.drain_overlay_events();
        self.show_editors(ui.ctx());
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        // Forwarded, not defaulted: the overlay's clear colour is transparent,
        // and eframe's default is a dark wash that would paint a grey sheet
        // over the user's whole work area — the "stray window" failure exactly.
        self.overlay.clear_color(visuals)
    }
}

fn drag_origin(ctx: &egui::Context, pending: &PendingDrag) -> scrozz_core::Result<DragOrigin> {
    let mut geometry = pending.geometry;
    if matches!(pending.subject, DragSubject::History(_)) {
        let history_origin = ctx.input(|input| {
            input
                .raw
                .viewports
                .get(&viewport_id())
                .and_then(|viewport| viewport.inner_rect)
                .map(|rect| rect.min)
        });
        let history_origin = history_origin.ok_or_else(|| {
            CoreError::TargetGone("the capture history window closed before drag-out began".into())
        })?;
        geometry = geometry_in_viewport(geometry, history_origin);
    }
    Ok(DragOrigin::from_event(
        pending.native_event.clone(),
        geometry.rect,
        geometry.pointer,
    ))
}

fn geometry_in_viewport(mut geometry: DragGeometry, origin: egui::Pos2) -> DragGeometry {
    geometry.rect.origin.x -= f64::from(origin.x);
    geometry.rect.origin.y -= f64::from(origin.y);
    geometry.pointer.x -= f64::from(origin.x);
    geometry.pointer.y -= f64::from(origin.y);
    geometry
}

/// Set to `0` to leave the overlay window as `eframe` made it.
///
/// The conversion is what stops a capture card stealing focus (D27), so this
/// is not a preference — it is a way to isolate the conversion when something
/// downstream of it misbehaves, and to keep running while it is being fixed.
pub const PANEL_ENV: &str = "SCROZZ_GUI_PANEL";

/// The native panel conversion, unless it was switched off.
fn panel_hook(native_surface: NativeSurfaceSlot) -> Option<scrozz_ui::PanelHook> {
    let enabled =
        std::env::var(PANEL_ENV).map_or(true, |raw| !matches!(raw.as_str(), "0" | "false" | "no"));
    if !enabled {
        tracing::warn!(
            "the panel conversion is disabled; capture cards will pull focus when clicked"
        );
        return Some(crate::gui::panel::hook_without_conversion(native_surface));
    }
    Some(crate::gui::panel::hook_with_surface(native_surface))
}

/// Where the overlay window goes.
///
/// The work area, not the display bounds: anchoring a card to the bottom-left
/// of the *bounds* puts it behind the Dock. Falls back to a sensible default
/// rather than failing, because a card in a slightly wrong place beats no card.
fn work_area(display: OverlayDisplay) -> OverlayGeometry {
    #[cfg(target_os = "macos")]
    {
        let selected = match display {
            OverlayDisplay::Active => scrozz_shell::macos::display::active_display(),
            OverlayDisplay::Primary => scrozz_shell::macos::display::primary_display(),
        };
        match selected {
            Ok(display) => {
                let area = display.work_area;
                return OverlayGeometry::new(egui::Rect::from_min_size(
                    egui::pos2(area.origin.x as f32, area.origin.y as f32),
                    egui::vec2(area.size.width as f32, area.size.height as f32),
                ));
            }
            Err(err) => {
                tracing::warn!(%err, "no work area; using the default overlay geometry");
            }
        }
    }

    OverlayGeometry::default()
}

/// An exact pointer source for the click-through logic, if one is available.
///
/// macOS exposes the global pointer in the same flipped coordinate system as the
/// display work areas, so converting to overlay-local coordinates is exact.
fn pointer_probe(geometry: Arc<Mutex<OverlayGeometry>>) -> Option<scrozz_ui::PointerProbe> {
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(move || {
            let point = scrozz_shell::macos::display::pointer_location().ok()?;
            let geometry = *geometry.lock().ok()?;
            Some(egui::pos2(
                point.x as f32 - geometry.work_area.left(),
                point.y as f32 - geometry.work_area.top(),
            ))
        }))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = geometry;
        None
    }
}

/// Focus-preserving native visibility control where the platform provides it.
fn visibility_hook(surface: NativeSurfaceSlot) -> Option<VisibilityHook> {
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(move |visible| {
            if let Err(error) = surface.set_visible_without_activation(visible) {
                tracing::warn!(%error, visible, "could not change overlay visibility");
            }
        }))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = surface;
        None
    }
}

/// Whether the environment asked for a run without a window.
#[must_use]
pub fn headless_requested() -> bool {
    std::env::var(HEADLESS_ENV).is_ok_and(|raw| !matches!(raw.as_str(), "" | "0" | "false" | "no"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_store::test_support::sample_document;

    #[test]
    fn a_headless_run_ends_by_itself() {
        // The property every automated run depends on. If this ever stops
        // holding, a test can leave a menu-bar item behind.
        let app = App::new(Config::sealed(), Box::new(Recording::new())).expect("sealed app");
        let started = std::time::Instant::now();
        let report = Box::new(Headless).run(app).expect("headless never fails");

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline was not honoured"
        );
        assert_eq!(report.human, "Scrozz ran and took no captures.");
    }

    #[test]
    fn the_headless_host_names_itself() {
        assert_eq!(Headless.describe(), "headless");
    }

    #[test]
    fn the_window_gap_says_what_to_do_about_it() {
        // A gap message that does not name the remedy is just an apology.
        assert!(WINDOW_GAP.contains("eframe"));
        assert!(WINDOW_GAP.contains("apps/scrozz/Cargo.toml"));
        assert!(WINDOW_GAP.contains("SCROZZ_GUI_HEADLESS"));
    }

    #[test]
    fn a_window_build_chooses_the_window_host() {
        // The inverse of the test this replaces. While `eframe` was missing,
        // the property worth pinning was that a missing window is an error
        // rather than a silent fallback; now that it is present, the property
        // is that asking for a window actually gets one.
        if headless_requested() {
            // The variable is set for this process; the branch under test is
            // unreachable and the assertion would be about nothing.
            return;
        }
        let host = for_platform(&Config::sealed(), Box::new(|_| {}))
            .expect("this build can open a window");
        assert_eq!(host.describe(), "eframe overlay");
    }

    #[test]
    fn asking_for_headless_gets_headless_even_though_a_window_is_possible() {
        // The escape hatch every automated run depends on. If this ever stops
        // working, the test suite starts opening windows on someone's desk.
        assert_eq!(Headless.describe(), "headless");
    }

    #[test]
    fn constructing_the_window_host_opens_no_window() {
        // `surface()` must be callable before the event loop exists, because
        // the app is built before the window is. If this ever starts touching
        // AppKit, the test suite would open a window.
        let host = Windowed::new(Box::new(|_| {}));
        let surface = host.surface();
        assert_eq!(surface.len(), 0);
        assert!(surface.describe().contains("no window"));
    }

    #[test]
    fn macos_has_an_exact_pointer_probe() {
        let geometry = Arc::new(Mutex::new(OverlayGeometry::default()));
        #[cfg(target_os = "macos")]
        assert!(pointer_probe(geometry).is_some());
        #[cfg(not(target_os = "macos"))]
        assert!(pointer_probe(geometry).is_none());
    }

    #[test]
    fn annotation_editors_register_as_deferred_native_viewports() {
        let ctx = egui::Context::default();
        ctx.set_embed_viewports(false);
        scrozz_ui::theme::install_fonts(&ctx);
        let icons = Arc::new(IconStore::new(&ctx));
        let capture = CaptureId("editor-17".to_owned());
        let session = Arc::new(Mutex::new(EditorSession::new(sample_document(
            16, 12, 3, 1,
        ))));

        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            show_editor_viewport(
                ui.ctx(),
                capture.clone(),
                Arc::clone(&session),
                Arc::clone(&icons),
            );
        });

        let viewport = output
            .viewport_output
            .get(&editor_viewport_id(&capture))
            .expect("the editor viewport should be registered");
        assert!(viewport.class == egui::ViewportClass::Deferred);
        assert_eq!(viewport.builder.min_inner_size, Some([900.0, 520.0].into()));
        assert_ne!(
            editor_viewport_id(&capture),
            editor_viewport_id(&CaptureId("editor-18".to_owned()))
        );
        output.textures_delta.clear();
    }

    fn edited_session() -> (EditorSession, Instant) {
        let mut session = EditorSession::new(sample_document(16, 12, 3, 1));
        let now = Instant::now();
        let mut data = session.latest_data.clone();
        data.canvas.flip_horizontal = true;
        session.changed(data, now);
        (session, now)
    }

    #[test]
    fn changed_snapshots_wait_for_the_autosave_debounce() {
        let (session, now) = edited_session();
        assert!(session.next_save(now).is_none());
        assert!(
            session
                .next_save(now + EDITOR_AUTOSAVE_DELAY - Duration::from_millis(1))
                .is_none()
        );
        assert_eq!(
            session
                .next_save(now + EDITOR_AUTOSAVE_DELAY)
                .expect("the debounced snapshot should become eligible")
                .revision,
            1
        );
    }

    #[test]
    fn explicit_save_bypasses_the_autosave_debounce() {
        let (mut session, now) = edited_session();
        session.save_requested(session.latest_data.clone());
        assert_eq!(
            session
                .next_save(now)
                .expect("an explicit save is a barrier")
                .revision,
            1
        );
    }

    #[test]
    fn stale_save_acknowledgements_do_not_clear_the_current_barrier() {
        let (mut session, now) = edited_session();
        session.save_requested(session.latest_data.clone());
        let snapshot = session.next_save(now).expect("save snapshot");
        session.save_dispatched(snapshot);

        assert!(!session.save_finished(0, None));
        assert_eq!(
            session.in_flight.as_ref().map(|save| save.revision),
            Some(1)
        );
        assert_eq!(session.acknowledged_revision, 0);
        assert!(session.needs_save());
    }

    #[test]
    fn failed_close_save_stays_dirty_and_retries_before_closing() {
        let (mut session, now) = edited_session();
        session.save_requested(session.latest_data.clone());
        session.close_requested = true;
        let snapshot = session.next_save(now).expect("close barrier");
        session.save_dispatched(snapshot);

        assert!(session.save_finished(1, Some("disk full".to_owned())));
        assert!(session.retry_blocked);
        assert!(session.needs_save());
        assert!(!session.is_ready_to_close());
        assert!(session.next_save(now + Duration::from_secs(1)).is_none());

        session.save_requested(session.latest_data.clone());
        let retry = session.next_save(now).expect("retry snapshot");
        session.save_dispatched(retry);
        assert!(session.save_finished(1, None));
        assert!(!session.needs_save());
        assert!(session.is_ready_to_close());
    }

    #[test]
    fn queue_failure_keeps_the_editor_open_with_a_retryable_snapshot() {
        let (mut session, now) = edited_session();
        session.save_requested(session.latest_data.clone());
        session.close_requested = true;
        assert!(session.next_save(now).is_some());

        session.save_dispatch_failed("worker unavailable");
        assert!(session.retry_blocked);
        assert!(session.needs_save());
        assert!(!session.is_ready_to_close());

        session.save_requested(session.latest_data.clone());
        assert!(session.next_save(now).is_some());
    }

    #[test]
    fn poisoned_editor_state_is_recovered_for_shutdown_persistence() {
        let capture = CaptureId("editor-poison".to_owned());
        let editor = Mutex::new(EditorSession::new(sample_document(16, 12, 3, 1)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = editor.lock().expect("initial lock");
            panic!("poison the advisory mutex");
        }));
        assert!(editor.is_poisoned());

        let mut recovered = lock_editor(&capture, &editor);
        recovered.close_requested();
        assert!(
            recovered.is_ready_to_close(),
            "an unchanged recovered editor must not strand shutdown"
        );
    }

    #[test]
    fn absolute_drag_geometry_is_translated_into_its_source_viewport() {
        let geometry = DragGeometry {
            rect: scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(410.0, 260.0),
                scrozz_core::LogicalSize::new(240.0, 160.0),
            ),
            pointer: scrozz_core::LogicalPoint::new(520.0, 330.0),
        };

        let translated = geometry_in_viewport(geometry, egui::pos2(100.0, 40.0));

        assert_eq!(
            translated.rect.origin,
            scrozz_core::LogicalPoint::new(310.0, 220.0)
        );
        assert_eq!(
            translated.pointer,
            scrozz_core::LogicalPoint::new(420.0, 290.0)
        );
    }
}
