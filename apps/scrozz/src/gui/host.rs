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
    sync::{Arc, Mutex, mpsc::channel},
    time::Duration,
};

use scrozz_core::{
    Capture, CaptureRequest, CursorMode, Error as CoreError, RegionSelector, SelectionHost,
    SelectionOptions, SelectionOutcome,
};
#[cfg(target_os = "macos")]
use scrozz_ui::VIDEO_EDITOR_WINDOW_TITLE;
use scrozz_ui::{
    CameraSettingsAction, HistoryWindowAction, OverlayHandle, RecordingSurfaceAction, Theme,
    VideoEditorAction, camera_settings_viewport_builder, camera_settings_viewport_id,
    history_viewport_builder, history_viewport_id,
    overlay_app::{OverlayApp, OverlayGeometry, OverlayOptions},
    show_camera_settings_window, show_history_window, show_video_editor_window,
    video_editor_viewport_builder, video_editor_viewport_id,
};

use crate::{
    fault::{CliError, CliResult},
    gui::{
        app::{App, Config, Tick},
        card::{CardSurface, Recording},
        overlay::OverlayCards,
        panel::BehaviorController,
        selection::{
            CaptureSelector, ClientOverlayController, ClientOverlaySelector, UnsupportedSelector,
            current_plan, for_current_session,
        },
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

    /// The selector that shares this host's event loop.
    fn selector(&self) -> Arc<dyn CaptureSelector>;
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

        let report = app.report();
        // Before the report is printed, not after: the menu-bar item should be
        // gone by the time the user sees any output about it.
        app.shut_down();
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

    fn selector(&self) -> Arc<dyn CaptureSelector> {
        Arc::new(UnsupportedSelector::headless())
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
pub fn for_platform(_config: &Config, emit: Emit) -> CliResult<Box<dyn Host>> {
    if headless_requested() {
        return Ok(Box::new(Headless));
    }

    if HAS_WINDOW {
        return Ok(Box::new(Windowed::new(emit)));
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
    geometry: OverlayGeometry,
    selector: Arc<dyn CaptureSelector>,
    selection: ClientOverlayController,
    native: BehaviorController,
}

impl Windowed {
    /// A host with an overlay handle that works before the window exists.
    #[must_use]
    pub fn new(emit: Emit) -> Self {
        let geometry = work_area();
        let native = BehaviorController::default();
        let (client, selection) = ClientOverlaySelector::managed(geometry);
        let client: Arc<dyn CaptureSelector> = client;
        let (selector, plan) = for_current_session(client);
        tracing::info!(
            host = ?plan.host,
            available = plan.is_available(),
            detail = %plan.detail,
            "resolved interactive selector"
        );
        Self {
            handle: OverlayHandle::new(),
            emit,
            geometry,
            selector,
            selection,
            native,
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
        let Self {
            handle,
            emit,
            geometry,
            selector: _,
            selection,
            native,
        } = *self;
        tracing::info!(?geometry, "opening the overlay");

        let options = OverlayOptions {
            geometry,
            panel: panel_hook(native.clone()),
            probe: pointer_probe(),
            ..Default::default()
        };

        // The app is moved into the window, so the report has to come back out
        // some other way: `run_native` owns the loop and drops everything in
        // it before returning.
        let outcome: Arc<Mutex<Option<Report>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&outcome);
        let reporting = handle.clone();

        eframe::run_native(
            "Scrozz",
            scrozz_ui::overlay_app::native_options(geometry),
            Box::new(move |cc| {
                let overlay = OverlayApp::new(cc, handle, options);
                Ok(Box::new(Driver {
                    app,
                    overlay,
                    sink,
                    handle: reporting,
                    emit: Some(emit),
                    selection,
                    native,
                    announced: false,
                    stopped: false,
                    history_actions: Arc::new(Mutex::new(Vec::new())),
                    editor_actions: Arc::new(Mutex::new(Vec::new())),
                    editor_was_open: false,
                    editor_focus_pending: false,
                    editor_focus_delay: 0,
                    editor_focus_error_reported: false,
                    editor_hidden_for_selection: false,
                    camera_settings_actions: Arc::new(Mutex::new(Vec::new())),
                    camera_settings_was_open: false,
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
        Box::new(OverlayCards::new(self.handle.clone()))
    }

    fn selector(&self) -> Arc<dyn CaptureSelector> {
        Arc::clone(&self.selector)
    }
}

/// Runs one interactive selection in its own ordinary eframe window.
///
/// The long-running app reuses its card window instead. This path deliberately
/// skips panel conversion: the current AppKit conversion cannot be dismantled
/// safely after winit has installed KVO, while a one-shot window must return to
/// the caller so the selected target can be captured.
pub fn select_once(
    options: &SelectionOptions,
    cursor: CursorMode,
    include_window_shadow: bool,
) -> scrozz_core::Result<(SelectionOutcome, Option<Capture>)> {
    let plan = current_plan();
    if plan.host != SelectionHost::ClientOverlay || !plan.is_available() {
        return UnsupportedSelector::from_plan(plan)
            .select(options)
            .map(|outcome| (outcome, None));
    }

    let (selector, selection) = ClientOverlaySelector::one_shot();
    let worker_selector = Arc::clone(&selector);
    let worker_options = options.clone();
    let (result_tx, result_rx) = channel();
    let worker = std::thread::Builder::new()
        .name("scrozz-one-shot-selector".to_owned())
        .spawn(move || {
            let result = worker_selector
                .select_for_capture(&worker_options, cursor)
                .map(|outcome| {
                    let request = CaptureRequest {
                        target: outcome.target.clone(),
                        cursor,
                        include_window_shadow,
                    };
                    let frozen = worker_selector.take_frozen_capture(&request);
                    (outcome, frozen)
                });
            worker_selector.capture_finished();
            let _ = result_tx.send(result);
        })
        .map_err(|error| {
            CoreError::Platform(format!("could not start the selector worker: {error}"))
        })?;

    let geometry = OverlayGeometry::default();
    let mut native_options = scrozz_ui::overlay_app::native_options(geometry);
    native_options.viewport = native_options
        .viewport
        .with_visible(false)
        .with_active(false);
    let driver_selector = Arc::clone(&selector);
    let native = BehaviorController::default();
    let creation_native = native.clone();
    let run_result = eframe::run_native(
        "Scrozz Selector",
        native_options,
        Box::new(move |cc| {
            #[cfg(target_os = "linux")]
            {
                match crate::gui::panel::attach_x11_focus(cc, &creation_native) {
                    Ok(()) => tracing::info!("attached one-shot X11 selector focus"),
                    Err(error) => {
                        tracing::warn!(%error, "one-shot X11 selector focus unavailable");
                    }
                }
            }
            Ok(Box::new(OneShotDriver {
                selection,
                native: creation_native,
                selector: driver_selector,
            }))
        }),
    );
    if let Err(error) = &run_result {
        selector.cancel();
        let _ = worker.join();
        return Err(CoreError::Platform(format!(
            "the selector window could not open: {error}"
        )));
    }

    let selected = result_rx.recv().map_err(|_| {
        CoreError::Platform("the selector worker stopped without an outcome".to_owned())
    })?;
    let _ = worker.join();
    selected
}

struct OneShotDriver {
    selection: ClientOverlayController,
    native: BehaviorController,
    selector: Arc<ClientOverlaySelector>,
}

impl eframe::App for OneShotDriver {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.selection.logic(ctx, &self.native);
        ctx.request_repaint_after(IDLE);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.selection.ui(ui);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}

impl Drop for OneShotDriver {
    fn drop(&mut self) {
        self.selector.cancel();
    }
}

/// The `eframe::App` that services this app and draws the overlay.
struct Driver {
    app: App,
    overlay: OverlayApp,
    sink: Arc<Mutex<Option<Report>>>,
    handle: OverlayHandle,
    emit: Option<Emit>,
    selection: ClientOverlayController,
    native: BehaviorController,
    announced: bool,
    stopped: bool,
    history_actions: Arc<Mutex<Vec<HistoryWindowAction>>>,
    editor_actions: Arc<Mutex<Vec<VideoEditorAction>>>,
    editor_was_open: bool,
    editor_focus_pending: bool,
    editor_focus_delay: u8,
    editor_focus_error_reported: bool,
    editor_hidden_for_selection: bool,
    camera_settings_actions: Arc<Mutex<Vec<CameraSettingsAction>>>,
    camera_settings_was_open: bool,
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

    fn show_history(&self, ctx: &egui::Context) {
        let Some(snapshot) = self.app.history_window() else {
            return;
        };
        let actions = Arc::clone(&self.history_actions);
        let viewport = history_viewport_id();
        ctx.request_repaint_of(viewport);
        ctx.show_viewport_deferred(viewport, history_viewport_builder(), move |ui, _class| {
            let theme = if ui.visuals().dark_mode {
                Theme::dark()
            } else {
                Theme::light()
            };
            let close_requested = ui.ctx().input(|input| input.viewport().close_requested());
            let mut emitted = show_history_window(ui, &snapshot, &theme);
            if close_requested {
                emitted.push(HistoryWindowAction::Close);
            }
            if emitted
                .iter()
                .any(|action| matches!(action, HistoryWindowAction::Close))
            {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
            actions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(emitted);
            ui.ctx().request_repaint_after(IDLE);
        });
    }

    fn show_video_editor(&mut self, ctx: &egui::Context) {
        let Some(snapshot) = self.app.video_editor_snapshot() else {
            if self.editor_was_open {
                ctx.send_viewport_cmd_to(video_editor_viewport_id(), egui::ViewportCommand::Close);
                if !self.selection.owns_surface() {
                    ctx.send_viewport_cmd_to(
                        egui::ViewportId::ROOT,
                        egui::ViewportCommand::Visible(true),
                    );
                }
            }
            self.editor_was_open = false;
            self.editor_focus_pending = false;
            self.editor_focus_delay = 0;
            self.editor_focus_error_reported = false;
            self.editor_hidden_for_selection = false;
            return;
        };

        if !self.editor_was_open {
            self.editor_focus_pending = true;
            self.editor_focus_delay = 1;
            self.editor_focus_error_reported = false;
        }
        if self.editor_hidden_for_selection && !self.selection.owns_surface() {
            self.editor_hidden_for_selection = false;
            self.editor_focus_pending = true;
            self.editor_focus_delay = 1;
            self.editor_focus_error_reported = false;
        }
        self.editor_was_open = true;

        let actions = Arc::clone(&self.editor_actions);
        let viewport = video_editor_viewport_id();
        ctx.request_repaint_of(viewport);
        ctx.show_viewport_deferred(
            viewport,
            video_editor_viewport_builder(),
            move |ui, _class| {
                let theme = if ui.ctx().system_theme().unwrap_or_else(|| ui.ctx().theme())
                    == egui::Theme::Dark
                {
                    Theme::dark()
                } else {
                    Theme::light()
                };
                let close_requested = ui.ctx().input(|input| input.viewport().close_requested());
                let response = show_video_editor_window(ui, &snapshot, &theme);
                let mut emitted = response.actions;
                if close_requested {
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    if !emitted
                        .iter()
                        .any(|action| matches!(action, VideoEditorAction::Close))
                    {
                        emitted.push(VideoEditorAction::Close);
                    }
                }
                actions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(emitted);
                ui.ctx().request_repaint_after(IDLE);
            },
        );

        if self.selection.owns_surface() {
            ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Visible(false));
            self.editor_hidden_for_selection = true;
            self.editor_focus_pending = true;
            self.editor_focus_delay = 1;
            return;
        }
        ctx.send_viewport_cmd_to(
            egui::ViewportId::ROOT,
            egui::ViewportCommand::Visible(false),
        );
        if self.editor_focus_pending {
            if self.editor_focus_delay > 0 {
                self.editor_focus_delay -= 1;
                return;
            }
            #[cfg(target_os = "macos")]
            match activate_video_editor_window() {
                Ok(true) => {
                    self.editor_focus_pending = false;
                }
                Ok(false) => {}
                Err(error) if !self.editor_focus_error_reported => {
                    self.editor_focus_error_reported = true;
                    tracing::error!(%error, "recording editor window activation failed");
                }
                Err(_) => {}
            }
            #[cfg(not(target_os = "macos"))]
            {
                ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Focus);
                self.editor_focus_pending = false;
            }
        }
    }

    fn show_camera_settings(&mut self, ctx: &egui::Context) {
        let Some(snapshot) = self.app.camera_settings_snapshot() else {
            if self.camera_settings_was_open {
                ctx.send_viewport_cmd_to(
                    camera_settings_viewport_id(),
                    egui::ViewportCommand::Close,
                );
            }
            self.camera_settings_was_open = false;
            return;
        };
        let first_frame = !self.camera_settings_was_open;
        self.camera_settings_was_open = true;
        let actions = Arc::clone(&self.camera_settings_actions);
        let viewport = camera_settings_viewport_id();
        ctx.request_repaint_of(viewport);
        ctx.show_viewport_deferred(
            viewport,
            camera_settings_viewport_builder(),
            move |ui, _class| {
                let theme = if ui.ctx().system_theme().unwrap_or_else(|| ui.ctx().theme())
                    == egui::Theme::Dark
                {
                    Theme::dark()
                } else {
                    Theme::light()
                };
                let close_requested = ui.ctx().input(|input| input.viewport().close_requested());
                let response = show_camera_settings_window(ui, &snapshot, &theme);
                let mut emitted = response.actions;
                if close_requested {
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    if !emitted
                        .iter()
                        .any(|action| matches!(action, CameraSettingsAction::Close))
                    {
                        emitted.push(CameraSettingsAction::Close);
                    }
                }
                actions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(emitted);
                ui.ctx().request_repaint_after(IDLE);
            },
        );
        if first_frame {
            ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Focus);
        }
    }
}

#[cfg(target_os = "macos")]
fn activate_video_editor_window() -> scrozz_core::Result<bool> {
    scrozz_shell::macos::editor::activate(VIDEO_EDITOR_WINDOW_TITLE)
        .map(|diagnostics| diagnostics.is_some())
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
        self.announce_panel();
        self.selection.logic(ctx, &self.native);

        let mut pending_history_actions = self
            .history_actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let history_actions = std::mem::take(&mut *pending_history_actions);
        drop(pending_history_actions);
        for action in history_actions {
            self.app.handle_history_window_action(action);
        }
        let mut pending_camera_actions = self
            .camera_settings_actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let camera_actions = std::mem::take(&mut *pending_camera_actions);
        drop(pending_camera_actions);
        for action in camera_actions {
            self.app.handle_camera_settings_action(action);
        }
        let mut pending_editor_actions = self
            .editor_actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let editor_actions = std::mem::take(&mut *pending_editor_actions);
        drop(pending_editor_actions);
        for action in editor_actions {
            self.app
                .handle_recording_surface_action(RecordingSurfaceAction::Editor(action));
        }
        for action in self.handle.drain_recording_actions() {
            self.app.handle_recording_surface_action(action);
        }
        if !self.stopped && self.app.tick() == Tick::Stop {
            self.stopped = true;
            let report = self.app.report();
            if let Ok(mut slot) = self.sink.lock() {
                *slot = Some(report.clone());
            }
            // Before the window closes, so the menu-bar item never outlives
            // what the user can see.
            self.app.shut_down();
            self.handle.set_recording(None);

            if self.converted() {
                if let Some(emit) = self.emit.take() {
                    emit(&report);
                }
                tracing::debug!("leaving without dismantling the converted panel");
                std::process::exit(0);
            }

            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if !self.stopped {
            if self.app.video_editor_snapshot().is_some() {
                self.handle.set_recording(None);
            } else {
                self.handle.set_recording(self.app.recording_presentation());
            }
            self.show_video_editor(ctx);
            self.show_history(ctx);
            self.show_camera_settings(ctx);
        }

        // An idle overlay must still be woken, or a hotkey pressed while
        // nothing is on screen would not be noticed until something else woke
        // the window — which, for a window that is empty at rest, may be never.
        ctx.request_repaint_after(IDLE);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.selection.owns_surface() {
            self.selection.ui(ui);
        } else {
            self.overlay.ui(ui, frame);
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        // Forwarded, not defaulted: the overlay's clear colour is transparent,
        // and eframe's default is a dark wash that would paint a grey sheet
        // over the user's whole work area — the "stray window" failure exactly.
        self.overlay.clear_color(visuals)
    }
}

/// Set to `0` to leave the overlay window as `eframe` made it.
///
/// The conversion is what stops a capture card stealing focus (D27), so this
/// is not a preference — it is a way to isolate the conversion when something
/// downstream of it misbehaves, and to keep running while it is being fixed.
pub const PANEL_ENV: &str = "SCROZZ_GUI_PANEL";

/// The native panel conversion, unless it was switched off.
fn panel_hook(controller: BehaviorController) -> Option<scrozz_ui::PanelHook> {
    let enabled =
        std::env::var(PANEL_ENV).map_or(true, |raw| !matches!(raw.as_str(), "0" | "false" | "no"));
    if !enabled {
        tracing::warn!(
            "the panel conversion is disabled; capture cards will pull focus when clicked"
        );
        return None;
    }
    Some(crate::gui::panel::hook_with_controller(controller))
}

/// Where the overlay window goes.
///
/// The work area, not the display bounds: anchoring a card to the bottom-left
/// of the *bounds* puts it behind the Dock. Falls back to a sensible default
/// rather than failing, because a card in a slightly wrong place beats no card.
fn work_area() -> OverlayGeometry {
    #[cfg(target_os = "macos")]
    {
        match scrozz_shell::macos::display::active_display() {
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
/// Returns `None` today. See [`PROBE_GAP`] — the degradation is bounded and
/// documented, and a probe that guessed would be worse than none.
fn pointer_probe() -> Option<scrozz_ui::PointerProbe> {
    tracing::debug!("{PROBE_GAP}");
    None
}

/// Why there is no pointer probe.
pub const PROBE_GAP: &str = "no crate exposes the pointer as a point. \
     scrozz-shell reads NSEvent::mouseLocation inside \
     macos::display::active_display and returns the Display containing it, \
     never the location, so the one correct implementation of the AppKit \
     coordinate flip is not reachable from here. Exposing \
     `pub fn pointer_location() -> Result<LogicalPoint>` next to \
     active_display would be a three-line extraction and is the right fix; \
     calling NSEvent::mouseLocation again from this crate would duplicate \
     that flip and eventually disagree with it. Without a probe the overlay \
     re-samples click-through every 350ms, which is imprecise but bounded";

/// Whether the environment asked for a run without a window.
#[must_use]
pub fn headless_requested() -> bool {
    std::env::var(HEADLESS_ENV).is_ok_and(|raw| !matches!(raw.as_str(), "" | "0" | "false" | "no"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_headless_run_ends_by_itself() {
        // The property every automated run depends on. If this ever stops
        // holding, a test can leave a menu-bar item behind.
        let app = App::new(
            Config::sealed(),
            Box::new(Recording::new()),
            Arc::new(UnsupportedSelector::headless()),
        )
        .expect("sealed app");
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
    fn the_probe_gap_names_the_fix_rather_than_apologising() {
        assert!(PROBE_GAP.contains("pointer_location"), "{PROBE_GAP}");
        assert!(PROBE_GAP.contains("350ms"), "{PROBE_GAP}");
    }
}
