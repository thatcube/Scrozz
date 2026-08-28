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
    time::{Duration, Instant},
};

use scrozz_core::{
    Capture, CaptureRequest, CursorMode, Display, DisplayId, Error as CoreError, RegionSelector,
    SelectionHost, SelectionOptions, SelectionOutcome,
};
use scrozz_ui::{
    OverlayHandle,
    overlay_app::{OverlayApp, OverlayGeometry, OverlayOptions},
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
const WORK_AREA_REFRESH: Duration = Duration::from_millis(250);

type SharedGeometry = Arc<Mutex<OverlayGeometry>>;

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
    display_id: Option<DisplayId>,
    pointer_geometry: SharedGeometry,
    selector: Arc<dyn CaptureSelector>,
    selection: ClientOverlayController,
    native: BehaviorController,
}

impl Windowed {
    /// A host with an overlay handle that works before the window exists.
    #[must_use]
    pub fn new(emit: Emit) -> Self {
        let (geometry, display_id) = work_area();
        let pointer_geometry = Arc::new(Mutex::new(geometry));
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
            display_id,
            pointer_geometry,
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
            display_id,
            pointer_geometry,
            selector: _,
            selection,
            native,
        } = *self;
        tracing::info!(?geometry, "opening the overlay");

        let options = OverlayOptions {
            geometry,
            panel: panel_hook(native.clone()),
            probe: pointer_probe(Arc::clone(&pointer_geometry)),
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
                native.set_frame(logical_frame(geometry));
                Ok(Box::new(Driver {
                    app,
                    overlay,
                    sink,
                    handle: reporting,
                    emit: Some(emit),
                    selection,
                    native,
                    display_id,
                    pointer_geometry,
                    next_work_area_refresh: Instant::now(),
                    pending_native_frame: None,
                    announced: false,
                    stopped: false,
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

fn initialize_one_shot_context(ctx: &egui::Context) {
    scrozz_ui::theme::install_fonts(ctx);
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
                .select_for_capture(&worker_options, cursor, false)
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
            initialize_one_shot_context(&cc.egui_ctx);
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
    display_id: Option<DisplayId>,
    pointer_geometry: SharedGeometry,
    next_work_area_refresh: Instant,
    /// Work-area frame to apply natively on the pass after queued viewport
    /// commands have reached winit.
    pending_native_frame: Option<OverlayGeometry>,
    announced: bool,
    stopped: bool,
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
        if !self.selection.owns_surface()
            && let Some(geometry) = self.pending_native_frame.take()
        {
            self.native.set_frame(logical_frame(geometry));
        }
        let selector_owned_the_window = self.selection.owns_surface();
        self.selection.logic(ctx, &self.native);
        let selector_owns_the_window = self.selection.owns_surface();
        if selector_owned_the_window || selector_owns_the_window {
            self.overlay.invalidate_passthrough_cache();
            if selector_owned_the_window && !selector_owns_the_window {
                self.pending_native_frame = Some(self.overlay.geometry());
            }
        } else if Instant::now() >= self.next_work_area_refresh {
            self.next_work_area_refresh = Instant::now() + WORK_AREA_REFRESH;
            let geometry = refreshed_work_area(self.overlay.geometry(), &mut self.display_id);
            if geometry != self.overlay.geometry() {
                self.overlay.set_geometry(
                    geometry,
                    ctx,
                    &scrozz_ui::motion::Motion::from_context(ctx),
                );
                self.pending_native_frame = Some(geometry);
                self.selection.set_cards_geometry(geometry);
                if let Ok(mut current) = self.pointer_geometry.lock() {
                    *current = geometry;
                }
            }
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

            if self.converted() {
                if let Some(emit) = self.emit.take() {
                    emit(&report);
                }
                tracing::debug!("leaving without dismantling the converted panel");
                std::process::exit(0);
            }

            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
/// The card layout uses the work area, not the display bounds: anchoring a card
/// to the bottom-left of the *bounds* puts it behind the Dock. The transparent
/// viewport may extend below that safe area solely so shadows can fade naturally.
fn work_area() -> (OverlayGeometry, Option<DisplayId>) {
    #[cfg(target_os = "macos")]
    {
        match scrozz_shell::macos::display::active_display() {
            Ok(display) => {
                let geometry = geometry_for_display(&display);
                return (geometry, Some(display.id));
            }
            Err(err) => {
                tracing::warn!(%err, "no work area; using the default overlay geometry");
            }
        }
    }

    (OverlayGeometry::default(), None)
}

/// An exact pointer source for the click-through logic, if one is available.
///
fn pointer_probe(geometry: SharedGeometry) -> Option<scrozz_ui::PointerProbe> {
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(move || {
            let current = *geometry.lock().ok()?;
            scrozz_shell::macos::display::pointer_location()
                .ok()
                .map(|point| local_pointer(current, point))
        }))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = geometry;
        None
    }
}

fn local_pointer(geometry: OverlayGeometry, point: scrozz_core::LogicalPoint) -> egui::Pos2 {
    let origin = geometry.position();
    egui::pos2(point.x as f32 - origin.x, point.y as f32 - origin.y)
}

fn refreshed_work_area(
    current: OverlayGeometry,
    display_id: &mut Option<DisplayId>,
) -> OverlayGeometry {
    #[cfg(target_os = "macos")]
    {
        if let Ok(displays) = scrozz_shell::macos::display::displays()
            && let Some(display) = display_id
                .as_ref()
                .and_then(|id| display_by_id(&displays, id))
        {
            return geometry_for_display(display);
        }
        if let Ok(display) = scrozz_shell::macos::display::active_display() {
            let geometry = geometry_for_display(&display);
            *display_id = Some(display.id);
            return geometry;
        }
    }

    current
}

fn display_by_id<'a>(displays: &'a [Display], id: &DisplayId) -> Option<&'a Display> {
    displays.iter().find(|display| display.id == *id)
}

fn geometry_for_display(display: &Display) -> OverlayGeometry {
    let area = display.work_area;
    let work_area = egui::Rect::from_min_size(
        egui::pos2(area.origin.x as f32, area.origin.y as f32),
        egui::vec2(area.size.width as f32, area.size.height as f32),
    );
    let bounds_bottom = (display.bounds.origin.y + display.bounds.size.height) as f32;
    let viewport_bottom = (work_area.bottom() + scrozz_ui::card::SHADOW_BLEED).min(bounds_bottom);
    let viewport = egui::Rect::from_min_max(
        work_area.min,
        egui::pos2(work_area.right(), viewport_bottom),
    );
    OverlayGeometry::with_viewport(work_area, viewport)
}

fn logical_frame(geometry: OverlayGeometry) -> scrozz_core::LogicalRect {
    let area = geometry.viewport();
    scrozz_core::LogicalRect::new(
        scrozz_core::LogicalPoint::new(f64::from(area.min.x), f64::from(area.min.y)),
        scrozz_core::LogicalSize::new(f64::from(area.width()), f64::from(area.height())),
    )
}

/// Whether the environment asked for a run without a window.
#[must_use]
pub fn headless_requested() -> bool {
    std::env::var(HEADLESS_ENV).is_ok_and(|raw| !matches!(raw.as_str(), "" | "0" | "false" | "no"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize, ScaleFactor};

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
    fn pointer_probe_is_available_on_macos() {
        let probe = pointer_probe(Arc::new(Mutex::new(OverlayGeometry::default())));
        assert_eq!(probe.is_some(), cfg!(target_os = "macos"));
    }

    #[test]
    fn pointer_coordinates_are_relative_to_the_live_work_area() {
        let geometry = OverlayGeometry::new(egui::Rect::from_min_size(
            egui::pos2(100.0, 40.0),
            egui::vec2(800.0, 600.0),
        ));
        assert_eq!(
            local_pointer(geometry, scrozz_core::LogicalPoint::new(132.0, 91.0)),
            egui::pos2(32.0, 51.0)
        );
    }

    #[test]
    fn native_frame_matches_the_complete_transparent_viewport() {
        let work_area =
            egui::Rect::from_min_size(egui::pos2(120.0, 85.0), egui::vec2(1728.0, 951.0));
        let viewport =
            egui::Rect::from_min_size(egui::pos2(120.0, 85.0), egui::vec2(1728.0, 999.0));
        let geometry = OverlayGeometry::with_viewport(work_area, viewport);
        assert_eq!(
            logical_frame(geometry),
            LogicalRect::new(
                LogicalPoint::new(120.0, 85.0),
                LogicalSize::new(1728.0, 999.0),
            )
        );
    }

    #[test]
    fn mac_card_geometry_reserves_the_dock_but_not_the_shadow() {
        let display = Display {
            id: DisplayId("main".to_owned()),
            name: "Main".to_owned(),
            bounds: LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(1728.0, 1117.0),
            ),
            work_area: LogicalRect::new(
                LogicalPoint::new(0.0, 33.0),
                LogicalSize::new(1728.0, 950.0),
            ),
            scale: ScaleFactor::new(2.0),
            is_primary: true,
        };

        let geometry = geometry_for_display(&display);
        let layout = scrozz_ui::stack::StackLayout::new(
            geometry.local(),
            scrozz_ui::stack::CardMetrics::default(),
        );
        let bottom_card = layout.slot_rect(0);

        assert_eq!(geometry.position(), egui::pos2(0.0, 33.0));
        assert_eq!(geometry.viewport().bottom(), 1031.0);
        assert_eq!(geometry.work_area.bottom(), 983.0);
        assert_eq!(
            geometry.position().y + bottom_card.bottom(),
            981.0,
            "the card itself stays two points above the inferred Dock"
        );
        assert_eq!(
            geometry.size().y - bottom_card.bottom(),
            50.0,
            "the viewport leaves the two-point gap plus the full shadow bleed"
        );
    }

    #[test]
    fn work_area_refresh_follows_display_identity_across_coordinate_rebases() {
        let display = |id: &str, x: f64| Display {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds: LogicalRect::new(LogicalPoint::new(x, 0.0), LogicalSize::new(800.0, 600.0)),
            work_area: LogicalRect::new(LogicalPoint::new(x, 24.0), LogicalSize::new(800.0, 540.0)),
            scale: ScaleFactor::new(1.0),
            is_primary: id == "right",
        };
        let rebased = [display("left", -800.0), display("right", 0.0)];
        assert_eq!(
            display_by_id(&rebased, &DisplayId("left".to_owned()))
                .expect("the same display should survive the rebase")
                .bounds
                .origin
                .x,
            -800.0
        );
    }

    #[test]
    fn one_shot_selector_binds_fonts_before_keyboard_confirmation_paints() {
        let ctx = egui::Context::default();
        initialize_one_shot_context(&ctx);
        let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(360.0, 240.0));
        let mut output = ctx.run_ui(
            egui::RawInput {
                focused: true,
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |_| {},
        );
        output.textures_delta.clear();

        let bounds = LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(360.0, 240.0));
        let display = Display {
            id: DisplayId("main".to_owned()),
            name: "Main".to_owned(),
            bounds,
            work_area: bounds,
            scale: ScaleFactor::new(1.0),
            is_primary: true,
        };
        let mut selector = scrozz_ui::SelectionUi::new(
            SelectionOptions {
                remembered: Some(LogicalRect::new(
                    LogicalPoint::new(40.0, 50.0),
                    LogicalSize::new(160.0, 100.0),
                )),
                ..SelectionOptions::region()
            },
            vec![display],
            Vec::new(),
        );
        let key = |key, pressed| egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };

        let mut decision = scrozz_ui::SelectionDecision::Pending;
        let mut output = ctx.run_ui(
            egui::RawInput {
                focused: true,
                screen_rect: Some(screen_rect),
                events: vec![key(egui::Key::Tab, true), key(egui::Key::Tab, false)],
                ..Default::default()
            },
            |ui| decision = selector.update(ui),
        );
        output.textures_delta.clear();
        assert_eq!(decision, scrozz_ui::SelectionDecision::Pending);

        let mut output = ctx.run_ui(
            egui::RawInput {
                focused: true,
                screen_rect: Some(screen_rect),
                events: vec![key(egui::Key::Enter, true), key(egui::Key::Enter, false)],
                ..Default::default()
            },
            |ui| decision = selector.update(ui),
        );
        output.textures_delta.clear();
        assert!(matches!(
            decision,
            scrozz_ui::SelectionDecision::Selected(_)
        ));
    }
}
