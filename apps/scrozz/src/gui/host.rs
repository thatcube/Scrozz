//! Who owns the main loop.
//!
//! [`crate::gui::App`] deliberately has no loop of its own — see its module
//! documentation for why. A *host* is whatever supplies one: an `eframe` update
//! callback on a real desktop, or [`Headless`] in a terminal and in tests.
//!
//! Keeping this seam explicit is what stops a second event loop being invented
//! somewhere else later. There is exactly one, it is on the main thread, and it
//! calls [`App::tick`] — everything that blocks is already on a worker.

#[cfg(target_os = "macos")]
use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::PathBuf,
};
use std::{
    sync::{Arc, Mutex, mpsc::channel},
    time::{Duration, Instant},
};

use scrozz_core::{
    Capture, CaptureRequest, CursorMode, Display, DisplayId, DisplaySet, Error as CoreError,
    RegionSelector, ScaleFactor, SelectionHost, SelectionOptions, SelectionOutcome,
};
use scrozz_shell::{
    DragOrigin, DragSession, DragSource, NativeDragSource, native_drag_source,
    native_surface_for_window,
};
use scrozz_ui::{
    CameraSettingsAction, CloudSettingsWindow, RecentCapturesOverlayHandle, Theme,
    camera_settings_viewport_builder, camera_settings_viewport_id,
    history::{WINDOW_TITLE as HISTORY_WINDOW_TITLE, viewport_builder, viewport_id},
    recent_captures_overlay::{
        PinSupport, PinTopology, RecentCapturesOverlayApp, RecentCapturesOverlayGeometry,
        RecentCapturesOverlayOptions,
    },
    show_camera_settings_window,
};

use crate::{
    fault::{CliError, CliResult},
    gui::{
        app::{App, Config, EditorSnapshot, KeyboardOwner, PendingDrag, Tick},
        card::{CardSurface, Recording},
        panel::BehaviorController,
        pipeline::{DragGeometry, DragSubject},
        recent_captures_overlay::RecentCapturesOverlayCards,
        selection::{
            CaptureSelector, ClientOverlayController, ClientOverlaySelector, UnsupportedSelector,
            current_plan, for_current_session,
        },
    },
    report::Report,
};

/// Set to `1` to run without a window.
pub const HEADLESS_ENV: &str = "SCROZZ_GUI_HEADLESS";

/// Headless polling cadence when there is no native event loop to wake.
const IDLE: Duration = Duration::from_millis(16);
#[cfg(not(target_os = "macos"))]
const IDLE_FALLBACK_WAKE: Duration = Duration::from_millis(250);
/// Native preview playback and queued child-viewport input must advance at UI
/// cadence even while the shared transparent root is parked.
const VIDEO_EDITOR_TICK: Duration = Duration::from_millis(16);
/// How often an in-flight recording wakes the loop that advances its clock.
///
/// Fast enough that a stop feels immediate and a finished recording's card
/// appears at once; slow enough that an idle menu-bar app is still idle.
const RECORDING_TICK: Duration = Duration::from_millis(50);
const DISPLAY_REFRESH: Duration = Duration::from_millis(250);
#[cfg(target_os = "macos")]
const APPKIT_BOOTSTRAP_SETTLE: Duration = Duration::from_millis(250);

type SharedGeometry = Arc<Mutex<RecentCapturesOverlayGeometry>>;
const PARKED_ROOT_ORIGIN: f32 = -100_000.0;

#[cfg(target_os = "macos")]
const LIFECYCLE_DIAGNOSTIC_FILE_ENV: &str = "SCROZZ_LIFECYCLE_DIAGNOSTIC_FILE";

#[cfg(target_os = "macos")]
struct LifecycleDiagnostic {
    path: PathBuf,
    temporary: PathBuf,
    baseline: scrozz_shell::macos::activity::NativeActivitySnapshot,
}

#[cfg(target_os = "macos")]
impl LifecycleDiagnostic {
    fn start(
        baseline: scrozz_shell::macos::activity::NativeActivitySnapshot,
    ) -> scrozz_core::Result<Self> {
        let path = if let Some(path) = std::env::var_os(LIFECYCLE_DIAGNOSTIC_FILE_ENV) {
            PathBuf::from(path)
        } else {
            dirs::config_dir()
                .or_else(dirs::data_dir)
                .ok_or_else(|| {
                    CoreError::Storage(
                        "no platform directory is available for lifecycle diagnostics".to_owned(),
                    )
                })?
                .join("Scrozz")
                .join("last-lifecycle.json")
        };
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        let diagnostic = Self {
            path,
            temporary,
            baseline,
        };
        diagnostic.persist("starting", None, false, RootSurfaceMode::Hidden)?;
        Ok(diagnostic)
    }

    fn persist(
        &self,
        state: &str,
        reason: Option<crate::gui::app::ExitReason>,
        lease_active: bool,
        root_mode: RootSurfaceMode,
    ) -> scrozz_core::Result<()> {
        let activity = scrozz_shell::macos::activity::snapshot().since(self.baseline);
        let document = serde_json::json!({
            "schema": 1,
            "pid": std::process::id(),
            "state": state,
            "exit_reason": reason.map(crate::gui::app::ExitReason::label),
            "automatic_termination_lease_active": lease_active,
            "root_mode": format!("{root_mode:?}"),
            "native_activity": {
                "screen_preflights": activity.screen_preflights,
                "screen_requests": activity.screen_requests,
                "display_enumerations": activity.display_enumerations,
                "pointer_samples": activity.pointer_samples,
                "root_redraws": activity.root_redraws,
                "automatic_termination_disables": activity.automatic_termination_disables,
                "automatic_termination_enables": activity.automatic_termination_enables,
            },
        });
        let bytes = serde_json::to_vec_pretty(&document).map_err(|error| {
            CoreError::Storage(format!("could not encode lifecycle diagnostic: {error}"))
        })?;
        let parent = self.path.parent().ok_or_else(|| {
            CoreError::Storage(format!(
                "lifecycle diagnostic path has no parent: {}",
                self.path.display()
            ))
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            CoreError::Storage(format!(
                "could not create lifecycle diagnostic directory {}: {error}",
                parent.display()
            ))
        })?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.temporary)
            .map_err(|error| {
                CoreError::Storage(format!(
                    "could not open lifecycle diagnostic {}: {error}",
                    self.temporary.display()
                ))
            })?;
        file.write_all(&bytes).map_err(|error| {
            CoreError::Storage(format!(
                "could not write lifecycle diagnostic {}: {error}",
                self.temporary.display()
            ))
        })?;
        file.sync_all().map_err(|error| {
            CoreError::Storage(format!(
                "could not sync lifecycle diagnostic {}: {error}",
                self.temporary.display()
            ))
        })?;
        drop(file);
        scrozz_shell::replace_file(&self.temporary, &self.path)
    }
}

/// Something that can drive an [`App`] to completion.
/// Writes the final report the way `main` would have.
///
/// Needed because the windowed host owns the app until eframe has completed its
/// native teardown, so the final report crosses that ownership boundary.
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

    /// Whether this host can present and service the permission viewport.
    fn supports_permission_ui(&self) -> bool;
}

fn card_surface_geometry(
    base: RecentCapturesOverlayGeometry,
    card_count: usize,
) -> RecentCapturesOverlayGeometry {
    card_surface_geometry_with_settings(
        base,
        card_count,
        scrozz_ui::RecentCapturesOverlaySettings::default(),
    )
}

fn card_surface_geometry_with_settings(
    base: RecentCapturesOverlayGeometry,
    card_count: usize,
    settings: scrozz_ui::RecentCapturesOverlaySettings,
) -> RecentCapturesOverlayGeometry {
    let metrics = scrozz_ui::stack::CardMetrics {
        width: settings.card_width,
        height: settings.card_width / 1.6,
        ..scrozz_ui::stack::CardMetrics::default()
    };
    let layout =
        scrozz_ui::stack::StackLayout::with_placement(base.local(), metrics, settings.placement);
    let last = card_count.saturating_sub(1).min(layout.slots() - 1);
    let occupied = layout.slot_rect(0).union(layout.slot_rect(last));
    let viewport = occupied
        .translate(base.position().to_vec2())
        .expand(card_gesture_envelope())
        .intersect(base.viewport());
    RecentCapturesOverlayGeometry::with_content_viewport(base.work_area, viewport)
}

fn card_gesture_envelope() -> f32 {
    let gestures = scrozz_ui::stack::GestureConfig::default();
    gestures
        .dismiss_dist
        .max(gestures.dragout_dist)
        .max(gestures.collapse_dist)
        + scrozz_ui::card::SHADOW_BLEED
}

fn parked_native_options(geometry: RecentCapturesOverlayGeometry) -> eframe::NativeOptions {
    let mut options = scrozz_ui::recent_captures_overlay::native_options(geometry);
    options.viewport = options.viewport.with_visible(false);
    #[cfg(target_os = "macos")]
    {
        // Eframe orders the root in after its first rendered frame even when it
        // was constructed hidden. Keep that bootstrap frame outside every
        // display; Driver moves it to live geometry only for real content.
        options.viewport = options
            .viewport
            .with_position(egui::pos2(PARKED_ROOT_ORIGIN, PARKED_ROOT_ORIGIN))
            .with_inner_size(egui::vec2(1.0, 1.0));
    }
    options
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
            if matches!(app.tick(), Tick::Stop(_)) {
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

    fn supports_permission_ui(&self) -> bool {
        false
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
     scrozz-ui supplies the whole Recent Captures Overlay \
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
    handle: RecentCapturesOverlayHandle,
    emit: Emit,
    base_geometry: RecentCapturesOverlayGeometry,
    geometry: RecentCapturesOverlayGeometry,
    display_id: Option<DisplayId>,
    display_scale: Option<ScaleFactor>,
    pointer_geometry: SharedGeometry,
    selector: Arc<dyn CaptureSelector>,
    selection: ClientOverlayController,
    native: BehaviorController,
}

impl Windowed {
    /// A host with an overlay handle that works before the window exists.
    #[must_use]
    pub fn new(emit: Emit) -> Self {
        let (base_geometry, display_id, display_scale) = work_area().unwrap_or_else(|error| {
            tracing::warn!(%error, "native work area is not ready; deferring the required check until launch");
            (RecentCapturesOverlayGeometry::default(), None, None)
        });
        let geometry = card_surface_geometry(base_geometry, 1);
        let pointer_geometry = Arc::new(Mutex::new(geometry));
        let native = BehaviorController::default();
        let handle = RecentCapturesOverlayHandle::new();
        let selector_waker = handle.clone();
        let (client, selection) =
            ClientOverlaySelector::managed(geometry, Arc::new(move || selector_waker.wake()));
        let client: Arc<dyn CaptureSelector> = client;
        let (selector, plan) = for_current_session(client);
        tracing::info!(
            host = ?plan.host,
            available = plan.is_available(),
            detail = %plan.detail,
            "resolved interactive selector"
        );
        Self {
            handle,
            emit,
            base_geometry,
            geometry,
            display_id,
            display_scale,
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
            emit: _,
            base_geometry: _,
            geometry: _,
            display_id: _,
            display_scale: _,
            pointer_geometry,
            selector: _,
            mut selection,
            native,
        } = *self;
        let (base_geometry, display_id, display_scale) = work_area()?;
        let overlay_settings = app.recent_captures_overlay_settings();
        let geometry = card_surface_geometry_with_settings(base_geometry, 1, overlay_settings);
        if let Ok(mut current) = pointer_geometry.lock() {
            *current = geometry;
        }
        selection.set_cards_geometry(geometry);
        let (displays, active_display) = pin_displays(base_geometry);
        let pin_support = pin_support(&displays);
        let pin_lock_escapes = app.pin_lock_escapes().to_vec();
        tracing::info!(?geometry, "opening the overlay");
        #[cfg(target_os = "macos")]
        let display_changes = {
            let display_waker = handle.clone();
            match scrozz_shell::macos::display::DisplayChangeMonitor::new(Arc::new(move || {
                display_waker.wake();
            })) {
                Ok(monitor) => Some(monitor),
                Err(error) => {
                    tracing::warn!(%error, "display-change notifications are unavailable");
                    None
                }
            }
        };
        #[cfg(target_os = "macos")]
        let native_activity_start = scrozz_shell::macos::activity::snapshot();
        #[cfg(target_os = "macos")]
        let lifecycle_diagnostic = match LifecycleDiagnostic::start(native_activity_start) {
            Ok(diagnostic) => Some(diagnostic),
            Err(error) => {
                tracing::error!(%error, "lifecycle diagnostic could not be started");
                None
            }
        };
        let options = RecentCapturesOverlayOptions {
            geometry,
            settings: app.recent_captures_overlay_settings(),
            panel: Some(crate::gui::panel::hook_with_controller(native.clone())),
            probe: pointer_probe(Arc::clone(&pointer_geometry)),
            displays,
            active_display,
            pin_support,
            pin_lock_escapes,
            pin_topology_probe: Some(Arc::new(query_pin_topology)),
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
            parked_native_options(geometry),
            Box::new(move |cc| {
                let drag_source = match native_drag_source() {
                    Ok(source) => Some(source),
                    Err(err) => {
                        tracing::warn!("native drag-out is unavailable: {err}");
                        None
                    }
                };
                let overlay = RecentCapturesOverlayApp::new(cc, handle, options);
                // Gated on the GUI workflow existing at all: introducing a
                // feature that region selection cannot yet perform would be a
                // promise the app does not keep.
                let ocr_onboarding_memory = crate::gui::onboarding::OcrOnboardingMemory::system();
                let ocr_onboarding_visible = crate::gui::onboarding::workflow_available();
                native.set_frame(logical_frame(geometry));
                let (color_swatch_store, custom_swatches) =
                    match crate::color_swatches::CustomSwatchStore::default_location() {
                        Ok(store) => match store.load() {
                            Ok(colors) => (Some(store), colors),
                            Err(error) => {
                                tracing::warn!(%error, "custom colours could not be loaded");
                                (Some(store), Vec::new())
                            }
                        },
                        Err(error) => {
                            tracing::warn!(%error, "custom colours cannot be persisted");
                            (None, Vec::new())
                        }
                    };
                Ok(Box::new(Driver {
                    app,
                    overlay,
                    cloud_settings: CloudSettingsWindow::default(),
                    sink,
                    handle: reporting,
                    selection,
                    settings: scrozz_ui::settings::SettingsWindow::default(),
                    ocr_onboarding: scrozz_ui::OcrOnboarding,
                    ocr_onboarding_visible: ocr_onboarding_visible
                        && !ocr_onboarding_memory.has_seen(),
                    ocr_onboarding_memory,
                    permission: scrozz_ui::permission::PermissionWindow::default(),
                    permission_resume_armed: false,
                    editor: scrozz_ui::editor::EditorWindow::new(),
                    video_editor: VideoEditorWindow::default(),
                    video_editor_actions: Arc::new(Mutex::new(Vec::new())),
                    camera_settings_actions: Arc::new(Mutex::new(Vec::new())),
                    camera_settings_was_open: false,
                    editing: None,
                    color_picker: scrozz_shell::SystemColorPicker::default(),
                    color_picker_generation: None,
                    color_swatch_store,
                    custom_swatches,
                    native,
                    display_id,
                    base_geometry,
                    display_scale,
                    pointer_geometry,
                    #[cfg(not(target_os = "macos"))]
                    next_active_display_refresh: Instant::now(),
                    #[cfg(target_os = "macos")]
                    display_changes,
                    #[cfg(target_os = "macos")]
                    native_activity_start,
                    #[cfg(target_os = "macos")]
                    lifecycle_diagnostic,
                    #[cfg(target_os = "macos")]
                    automatic_termination: None,
                    #[cfg(target_os = "macos")]
                    appkit_bootstrap_frame_completed_at: None,
                    pin_panels: crate::gui::panel::PinPanels::default(),
                    root_mode: RootSurfaceMode::Hidden,
                    parked_root_bootstrap_pending: false,
                    parked_root_ordered_out: false,
                    card_arm: None,
                    shown_card_target: None,
                    startup_rehide: true,
                    announced: false,
                    stopped: false,
                    native_teardown_prepared: false,
                    drag_source,
                    active_drag: None,
                    retired_drags: Vec::new(),
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
        Box::new(
            RecentCapturesOverlayCards::new(self.handle.clone()).with_native(self.native.clone()),
        )
    }

    fn selector(&self) -> Arc<dyn CaptureSelector> {
        Arc::clone(&self.selector)
    }

    fn supports_permission_ui(&self) -> bool {
        true
    }
}

fn initialize_one_shot_context(ctx: &egui::Context) {
    scrozz_ui::recent_captures_overlay::install_native_point_scale(ctx);
    scrozz_ui::theme::install_fonts(ctx);
}

/// Runs one interactive selection in its own ordinary eframe window.
///
/// The long-running app reuses its card window instead. This one-shot path keeps
/// an ordinary winit window and never installs the retained native adapter.
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

    let geometry = RecentCapturesOverlayGeometry::default();
    let mut native_options = parked_native_options(geometry);
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
struct Editing {
    card: crate::gui::card::CardId,
    generation: u64,
    editor: scrozz_ui::editor::EditorUi,
}

struct Driver {
    app: App,
    overlay: RecentCapturesOverlayApp,
    /// The Sharing settings viewport, beside the aggregate's own Settings window.
    cloud_settings: CloudSettingsWindow,
    sink: Arc<Mutex<Option<Report>>>,
    handle: RecentCapturesOverlayHandle,
    selection: ClientOverlayController,
    settings: scrozz_ui::settings::SettingsWindow,
    /// The one-time text-recognition introduction, and the durable marker that
    /// decides whether it appears without being asked for.
    ocr_onboarding: scrozz_ui::OcrOnboarding,
    ocr_onboarding_visible: bool,
    ocr_onboarding_memory: crate::gui::onboarding::OcrOnboardingMemory,
    permission: scrozz_ui::permission::PermissionWindow,
    permission_resume_armed: bool,
    editor: scrozz_ui::editor::EditorWindow,
    /// The editor's document, and the card it came from.
    editing: Option<Editing>,
    /// The ordinary opaque recording-editor window.
    video_editor: VideoEditorWindow,
    /// Actions emitted by the deferred video viewport and drained by the next
    /// root logic pass.
    video_editor_actions: Arc<Mutex<Vec<scrozz_ui::VideoEditorAction>>>,
    /// Actions emitted by the deferred camera-settings viewport, drained on the
    /// next root pass so the camera is only ever touched from the app thread.
    camera_settings_actions: Arc<Mutex<Vec<CameraSettingsAction>>>,
    camera_settings_was_open: bool,
    color_picker: scrozz_shell::SystemColorPicker,
    /// Editor generation that opened the modeless system colour panel.
    color_picker_generation: Option<u64>,
    color_swatch_store: Option<crate::color_swatches::CustomSwatchStore>,
    custom_swatches: Vec<scrozz_annotate::Color>,
    native: BehaviorController,
    display_id: Option<DisplayId>,
    /// Complete display work-area geometry card slots are anchored against.
    base_geometry: RecentCapturesOverlayGeometry,
    /// Native pixels per logical point on the display owning the card root.
    display_scale: Option<ScaleFactor>,
    pointer_geometry: SharedGeometry,
    #[cfg(not(target_os = "macos"))]
    next_active_display_refresh: Instant,
    #[cfg(target_os = "macos")]
    display_changes: Option<scrozz_shell::macos::display::DisplayChangeMonitor>,
    #[cfg(target_os = "macos")]
    native_activity_start: scrozz_shell::macos::activity::NativeActivitySnapshot,
    #[cfg(target_os = "macos")]
    lifecycle_diagnostic: Option<LifecycleDiagnostic>,
    #[cfg(target_os = "macos")]
    automatic_termination: Option<scrozz_shell::macos::termination::AutomaticTerminationGuard>,
    #[cfg(target_os = "macos")]
    appkit_bootstrap_frame_completed_at: Option<Instant>,
    pin_panels: crate::gui::panel::PinPanels,
    /// Last presentation role applied to the shared native root window.
    root_mode: RootSurfaceMode,
    /// The visible off-screen root needs one committed frame before child registration.
    parked_root_bootstrap_pending: bool,
    /// The auxiliary viewport was registered before the parked root was ordered out.
    parked_root_ordered_out: bool,
    /// Hidden resize/scale barrier before the next first visible card frame.
    card_arm: Option<CardArm>,
    /// Geometry and scale used by the currently visible card framebuffer.
    shown_card_target: Option<CardSurfaceTarget>,
    /// Eframe orders the root in after its first rendered frame regardless of
    /// its initial visibility; reassert hidden once after that pass.
    startup_rehide: bool,
    announced: bool,
    stopped: bool,
    native_teardown_prepared: bool,
    drag_source: Option<NativeDragSource>,
    active_drag: Option<ActiveDrag>,
    retired_drags: Vec<DragSession>,
}

struct ActiveDrag {
    subject: DragSubject,
    session: DragSession,
}

impl Driver {
    #[cfg(target_os = "macos")]
    fn record_lifecycle(&self, state: &str) {
        let lease_active = self
            .automatic_termination
            .as_ref()
            .is_some_and(scrozz_shell::macos::termination::AutomaticTerminationGuard::is_active);
        if let Some(diagnostic) = &self.lifecycle_diagnostic
            && let Err(error) =
                diagnostic.persist(state, self.app.exit_reason(), lease_active, self.root_mode)
        {
            tracing::error!(%error, state, "lifecycle diagnostic could not be persisted");
        }
    }

    #[cfg(target_os = "macos")]
    fn acquire_automatic_termination_after_bootstrap(&mut self) -> Tick {
        if self.automatic_termination.is_some() {
            return Tick::Continue;
        }
        debug_assert!(
            self.appkit_bootstrap_frame_completed_at
                .is_some_and(|completed| completed.elapsed() >= APPKIT_BOOTSTRAP_SETTLE),
            "the app lease must follow AppKit's initial root-window bootstrap"
        );
        match scrozz_shell::macos::termination::AutomaticTerminationGuard::acquire() {
            Ok(guard) => {
                self.automatic_termination = Some(guard);
                self.record_lifecycle("running-post-bootstrap");
                Tick::Continue
            }
            Err(error) => {
                tracing::error!(%error, "automatic termination could not be inhibited");
                self.app.stop_for_native_lifecycle(error.to_string())
            }
        }
    }

    /// Draws the annotation editor's window while one is open.
    ///
    /// Copy and save go back through the worker rather than being written
    /// here: encoding a full-resolution PNG on the UI thread would stall the
    /// overlay, and the card's own copy already takes that route.
    ///
    /// The document lives on until the *window* closes, not until the editor
    /// says it is done, so reopening after an accidental Escape is impossible
    /// to distinguish from never having closed. Nothing is written back to the
    /// card: per D14 a capture's own pixels are never replaced by an annotated
    /// version unless the user explicitly saves one.
    ///
    /// Closing a dirty editor queues its scene graph before the card cache is
    /// released. The worker channel preserves that order, so a history-backed
    /// capture reopens with its editable annotations rather than a flattened
    /// approximation.
    fn show_editor(&mut self, ctx: &egui::Context) {
        use scrozz_ui::editor::Intent;

        // Tied to the viewport's lifetime rather than to `editing`, so the keys
        // come back even if the document is torn down by some other path.
        self.app
            .set_keyboard_owner(KeyboardOwner::Editor, self.editor.is_open());

        let picker_event = match self.color_picker.poll() {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(%error, "the system colour picker could not be read");
                None
            }
        };

        let Some(editing) = self.editing.as_mut() else {
            return;
        };
        let card = editing.card;
        let generation = editing.generation;
        let editor = &mut editing.editor;
        if let Some(event) = picker_event {
            match event {
                scrozz_shell::ColorPickerEvent::Changed([r, g, b, a])
                    if self.color_picker_generation == Some(generation) =>
                {
                    editor.apply_external_color(scrozz_annotate::Color::rgba(r, g, b, a));
                }
                scrozz_shell::ColorPickerEvent::Closed {
                    color: [r, g, b, a],
                    changed,
                } => {
                    if self.color_picker_generation == Some(generation) {
                        if changed {
                            editor.apply_external_color(scrozz_annotate::Color::rgba(r, g, b, a));
                        }
                        editor.remember_custom_color(scrozz_annotate::Color::rgba(r, g, b, a));
                        self.editor.request_foreground();
                    }
                    self.color_picker_generation = None;
                }
                scrozz_shell::ColorPickerEvent::Changed(_) => {}
            }
        }
        let mut intent = Intent::None;
        self.editor.show(ctx, |ui| {
            let got = editor.update(ui);
            if got != Intent::None {
                intent = got;
            }
        });

        match intent {
            Intent::None => {}
            Intent::ToggleSmartFrame => {}
            Intent::Close => self.editor.close(),
            Intent::Copy | Intent::Save => match editor.render() {
                Ok(rendered) => {
                    if intent == Intent::Copy {
                        self.app.copy_rendered(card, rendered);
                    } else {
                        self.app.save_rendered(card, rendered);
                    }
                }
                Err(error) => tracing::warn!(%error, "the annotated image could not be rendered"),
            },
            Intent::CustomColor => {
                let color = editor.state().stroke_color();
                match self.color_picker.open([color.r, color.g, color.b, color.a]) {
                    Ok(true) => {
                        self.color_picker_generation = Some(generation);
                        editor.native_color_picker_started();
                    }
                    Ok(false) => editor.open_custom_color_fallback(),
                    Err(error) => {
                        tracing::warn!(%error, "the system colour picker could not open");
                        editor.open_custom_color_fallback();
                    }
                }
            }
            Intent::AnalyzeSmartFrame {
                revision,
                data,
                cancellation,
            } => self
                .app
                .analyze_smart_frame(card, generation, revision, *data, cancellation),
            Intent::UpsertPreset(preset) => match self.app.upsert_smart_frame_preset(*preset) {
                Ok(presets) => editor.state_mut().set_custom_presets(presets),
                Err(error) => {
                    editor
                        .state_mut()
                        .set_custom_presets(self.app.smart_frame_presets().to_vec());
                    tracing::warn!(%error, "Smart Frame preset could not be saved");
                }
            },
            Intent::DeletePreset(preset_id) => {
                match self.app.delete_smart_frame_preset(&preset_id) {
                    Ok(presets) => editor.state_mut().set_custom_presets(presets),
                    Err(error) => {
                        editor
                            .state_mut()
                            .set_custom_presets(self.app.smart_frame_presets().to_vec());
                        tracing::warn!(%error, "Smart Frame preset could not be deleted");
                    }
                }
            }
            Intent::RequestSensitiveReview { revision, data } => {
                let _ = data;
                editor.deliver_sensitive_review(scrozz_annotate::SensitiveRegionReview {
                    revision,
                    ..Default::default()
                });
            }
        }

        self.app
            .prepare_editor(EditorSnapshot::new(card, generation, editor));
        if let Some(colors) = editor.take_custom_swatches_change() {
            if let Some(store) = &self.color_swatch_store
                && let Err(error) = store.save(&colors)
            {
                tracing::warn!(%error, "custom colours could not be saved");
            }
            self.custom_swatches = colors;
        }
        if self.color_picker.is_open() {
            ctx.request_repaint_after(IDLE);
        }
        if !self.editor.is_open() {
            if let Err(error) = self.color_picker.close() {
                tracing::warn!(%error, "the system colour picker could not close");
            }
            self.color_picker_generation = None;
            if editor.state().is_dirty() {
                self.app
                    .persist_editor(EditorSnapshot::new(card, generation, editor));
            }
            self.app
                .editor_closed(EditorSnapshot::new(card, generation, editor));
            self.editing = None;
        }
    }

    /// Says whether the native window owner supplied non-activating behavior.
    ///
    /// Logged on the first tick so a degraded ordinary-window session is
    /// explicit rather than silently claiming the panel contract.
    fn announce_panel(&mut self) {
        if self.announced {
            return;
        }
        let Some(report) = self.handle.panel_report() else {
            return;
        };
        self.announced = true;
        if report.non_activating {
            tracing::info!(
                detail = %report.detail,
                "the Recent Captures Overlay is non-activating"
            );
        } else {
            tracing::warn!(
                detail = %report.detail,
                "the Recent Captures Overlay is an ordinary window: clicking a card will steal focus"
            );
        }
    }

    fn refresh_display_state(&mut self) {
        #[cfg(target_os = "macos")]
        {
            let Ok(displays) = scrozz_shell::macos::display::displays() else {
                return;
            };
            let active = scrozz_shell::macos::display::pointer_location()
                .ok()
                .and_then(|point| scrozz_shell::macos::display::display_for_point(&displays, point))
                .cloned();
            let display = select_recent_captures_display(
                &displays,
                self.display_id.as_ref(),
                active.as_ref(),
                self.app
                    .recent_captures_overlay_settings()
                    .follow_active_display,
            );
            if let Some(display) = display {
                let geometry = geometry_for_display(display);
                let scale = Some(display.scale);
                self.display_id = Some(display.id.clone());
                if geometry != self.base_geometry || scale != self.display_scale {
                    self.base_geometry = geometry;
                    self.display_scale = scale;
                    let cards = self.desired_card_target().geometry;
                    self.selection.set_cards_geometry(cards);
                }
            }

            let displays = DisplaySet::new(displays);
            let support = pin_support(&displays);
            self.overlay.apply_pin_topology(PinTopology {
                displays,
                active_display: active.map(|display| display.id),
                support,
            });
        }

        #[cfg(not(target_os = "macos"))]
        {
            let (geometry, scale) = refreshed_work_area(
                self.base_geometry,
                self.display_scale,
                &mut self.display_id,
                self.app
                    .recent_captures_overlay_settings()
                    .follow_active_display,
            );
            if geometry != self.base_geometry || scale != self.display_scale {
                self.base_geometry = geometry;
                self.display_scale = scale;
                let cards = self.desired_card_target().geometry;
                self.selection.set_cards_geometry(cards);
            }
            self.overlay.refresh_pin_topology();
        }
    }

    fn native_display_parameters_changed(&mut self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.display_changes
                .as_mut()
                .is_some_and(scrozz_shell::macos::display::DisplayChangeMonitor::changed)
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    fn finish_shutdown(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.app.record_native_event_loop_exit();
        #[cfg(target_os = "macos")]
        self.record_lifecycle("shutdown-recorded");
        let report = self.app.report();
        if let Ok(mut slot) = self.sink.lock() {
            *slot = Some(report);
        }
        self.app.shut_down();
    }

    fn prepare_native_teardown(&mut self) {
        if self.native_teardown_prepared {
            return;
        }
        self.native_teardown_prepared = true;
        if let Err(error) = self.color_picker.close() {
            tracing::warn!(%error, "the system colour picker could not be ordered out");
        }
        self.pin_panels.prepare_for_winit_teardown();
        if let Err(error) = self.native.prepare_for_winit_teardown() {
            tracing::error!(%error, "the root window could not be returned to winit");
        }
    }

    fn desired_card_target(&self) -> CardSurfaceTarget {
        let count = self
            .app
            .showing()
            .max(self.handle.visible_card_count())
            .max(1);
        CardSurfaceTarget {
            geometry: card_surface_geometry_with_settings(
                self.base_geometry,
                count,
                self.handle.applied_settings(),
            ),
            scale: self.display_scale,
        }
    }

    fn prepare_card_surface(&mut self, ctx: &egui::Context, target: CardSurfaceTarget) {
        // Order out before changing position, size, or scale. A parked or old
        // card framebuffer must never be visible during this transition.
        self.startup_rehide = false;
        self.native
            .apply(&scrozz_shell::OverlayBehavior::hidden_surface());
        self.native.set_cursor(scrozz_shell::OverlayCursor::Arrow);
        self.native.set_visible(false);
        ctx.send_viewport_cmd(egui::ViewportCommand::ContentProtected(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));

        self.overlay.set_geometry(
            target.geometry,
            ctx,
            &scrozz_ui::motion::Motion::from_context(ctx),
        );
        self.native.set_frame(logical_frame(target.geometry));
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(
            target.geometry.position(),
        ));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(target.geometry.size()));
        self.selection.set_cards_geometry(target.geometry);
        if let Ok(mut current) = self.pointer_geometry.lock() {
            *current = target.geometry;
        }

        self.card_arm = Some(CardArm::new(target));
        self.shown_card_target = None;
        self.root_mode = RootSurfaceMode::ArmingCards;
        tracing::debug!(
            position = ?target.geometry.position(),
            size = ?target.geometry.size(),
            scale = ?target.scale.map(ScaleFactor::get),
            "arming hidden card surface"
        );
        ctx.request_repaint();
    }

    fn sync_card_visibility(&mut self, ctx: &egui::Context) {
        if self.handle.geometry_locked() && self.root_mode == RootSurfaceMode::Cards {
            return;
        }

        let target = self.desired_card_target();
        if self.root_mode == RootSurfaceMode::Cards
            && self.shown_card_target.is_some_and(|shown| {
                shown_card_surface_matches(target, shown, root_viewport_measurement(ctx))
            })
        {
            return;
        }

        if self
            .card_arm
            .as_ref()
            .is_none_or(|arm| arm.target != target)
        {
            self.prepare_card_surface(ctx, target);
            return;
        }

        let measurement = root_viewport_measurement(ctx);
        // Wayland exposes neither absolute outer nor inner rectangles. Its root
        // is created at the final card size (only macOS uses the 1×1 parked
        // bootstrap), so two hidden event-loop turns are the strongest honest
        // barrier available there.
        let ready = self.card_arm.as_mut().is_some_and(|arm| {
            if measurement.is_none() && cfg!(target_os = "linux") {
                arm.observe_ready(true)
            } else {
                arm.observe(measurement)
            }
        });
        if !ready {
            ctx.request_repaint();
            return;
        }
        let resolved_target = self
            .card_arm
            .expect("a ready card arm has a target")
            .resolved_target();

        // Install the complete card input contract before orderFront. Waiting
        // for the first card UI pass leaves the root click-through whenever a
        // child viewport is the only thing keeping eframe's UI loop alive.
        self.overlay.invalidate_passthrough_cache();
        reveal_card_surface(&self.native, ctx, resolved_target);
        ctx.request_repaint();

        self.card_arm = None;
        self.shown_card_target = Some(resolved_target);
        self.root_mode = RootSurfaceMode::Cards;
        tracing::debug!(
            position = ?resolved_target.geometry.position(),
            size = ?resolved_target.geometry.size(),
            scale = ?resolved_target.scale.map(ScaleFactor::get),
            "revealed settled card surface"
        );
    }

    fn sync_root_visibility(&mut self, ctx: &egui::Context, refresh_requested: bool) {
        let selector_visible = self.selection.wants_visible_selector();
        let history_open = self
            .app
            .history()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_visible();
        let auxiliary_open = self.settings.is_open()
            || self.editor.is_open()
            || self.app.video_editor_is_open()
            || self.video_editor.is_open()
            || self.camera_settings_was_open
            || self.app.permission_prompt().is_some()
            || history_open;
        let mode = root_surface_mode(
            selector_visible,
            self.selection.allows_card_surface(),
            self.handle.needs_visible_surface(),
            auxiliary_open,
        );
        if display_refresh_requested(
            refresh_requested,
            visible_surface_refresh_due(self.root_mode, mode),
            self.handle.has_pending_content(),
        ) {
            self.refresh_display_state();
        }
        if mode == RootSurfaceMode::Cards {
            self.sync_card_visibility(ctx);
            return;
        }

        self.card_arm = None;
        self.shown_card_target = None;
        let startup_rehide = self.startup_rehide && mode == RootSurfaceMode::Hidden;
        if mode == self.root_mode && !startup_rehide {
            return;
        }

        ctx.send_viewport_cmd(egui::ViewportCommand::ContentProtected(true));
        match mode {
            RootSurfaceMode::Selector => {
                self.startup_rehide = false;
                self.parked_root_bootstrap_pending = false;
                self.parked_root_ordered_out = false;
                self.native.set_visible(true);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.request_repaint();
            }
            RootSurfaceMode::Parked => {
                self.startup_rehide = false;
                self.parked_root_bootstrap_pending = parked_root_requires_bootstrap(self.root_mode);
                self.parked_root_ordered_out = false;
                self.native
                    .apply(&scrozz_shell::OverlayBehavior::hidden_surface());
                let frame = scrozz_core::LogicalRect::new(
                    scrozz_core::LogicalPoint::new(
                        f64::from(PARKED_ROOT_ORIGIN),
                        f64::from(PARKED_ROOT_ORIGIN),
                    ),
                    scrozz_core::LogicalSize::new(1.0, 1.0),
                );
                self.native.set_frame(frame);
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                    PARKED_ROOT_ORIGIN,
                    PARKED_ROOT_ORIGIN,
                )));
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(1.0, 1.0)));
                // eframe skips App::ui for an invisible root with no visible
                // descendants. Keep this 1x1 off-screen bootstrap visible for
                // one pass so the immediate Settings/editor viewport can be
                // registered, then order it out at the end of that UI pass.
                self.native.set_visible(true);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.request_repaint();
            }
            RootSurfaceMode::Hidden => {
                self.parked_root_bootstrap_pending = false;
                self.parked_root_ordered_out = false;
                self.native
                    .apply(&scrozz_shell::OverlayBehavior::hidden_surface());
                self.native.set_cursor(scrozz_shell::OverlayCursor::Arrow);
                self.native.set_visible(false);
                ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                if self.startup_rehide {
                    if ctx.cumulative_frame_nr() == 0 {
                        ctx.request_repaint();
                    } else {
                        self.startup_rehide = false;
                    }
                }
            }
            RootSurfaceMode::Cards | RootSurfaceMode::ArmingCards => {
                unreachable!("card modes are handled by the hidden framebuffer arming barrier")
            }
        }
        self.root_mode = mode;
    }

    #[cfg(target_os = "macos")]
    fn order_out_parked_root_after_auxiliary_registration(&mut self, ctx: &egui::Context) {
        if self.root_mode != RootSurfaceMode::Parked || self.parked_root_ordered_out {
            return;
        }
        self.native.set_visible(false);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.parked_root_ordered_out = true;
        self.record_lifecycle("auxiliary-registered-root-ordered-out");
    }

    fn show_capture_surface(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.selection.owns_surface() {
            self.selection.ui(ui);
            return;
        }

        eframe::App::ui(&mut self.overlay, ui, frame);

        // The editor pass comes first so a same-frame edit changes the
        // revision this lookup asks for. The overlay draw and native call
        // remain adjacent: once the card arms, nothing slow is allowed
        // between that event and the platform taking the held pointer.
        let editor = self
            .editing
            .as_ref()
            .map(|editing| EditorSnapshot::new(editing.card, editing.generation, &editing.editor));
        let started = self.app.pump_drag_starts_with_editor(editor);
        if started > 0 {
            tracing::debug!(started, "drag: began within the gesture frame");
        }
    }

    fn reconcile_pin_panels(&mut self, ctx: &egui::Context) {
        let native = self
            .pin_panels
            .reconcile(&self.handle.native_pin_requests());
        if native.pending_adoption {
            ctx.request_repaint_after(Duration::from_millis(32));
        }
        for failure in native.failures {
            self.handle.native_pin_failure(failure.pin, failure.reason);
        }
    }

    /// Draws the Sharing settings viewport and applies what it asks for.
    ///
    /// A viewport of its own rather than a pane inside the main Settings
    /// window: it owns transient credential fields that are zeroized when it
    /// closes, and those must not live for as long as the Settings window does.
    fn show_cloud_settings(&mut self, ctx: &egui::Context) {
        if !self.cloud_settings.is_open() {
            return;
        }
        let model = self.app.cloud_settings().clone();
        for event in self.cloud_settings.show(ctx, &model) {
            self.app.apply_cloud_settings(event);
        }
    }

    fn show_settings(&mut self, ctx: &egui::Context) {
        let edits = self.settings.show(
            ctx,
            scrozz_ui::settings::BuildInfo {
                version: crate::build_info::VERSION,
                build: crate::build_info::BUILD,
            },
            &self.app.shortcut_rows(),
            &self.app.after_capture_rows(),
            self.app.recording_settings_pane(),
            self.app.recent_captures_overlay_settings(),
        );
        self.app.set_keyboard_owner(
            KeyboardOwner::ShortcutRecorder,
            self.settings.is_recording(),
        );
        self.app.edit_shortcuts(&edits.shortcuts);
        self.app.edit_after_capture(&edits.after_capture);
        self.app.edit_recording_settings(&edits.recording);
        if let Some(settings) = edits.recent_captures_overlay {
            self.app.edit_recent_captures_overlay(settings);
        }
        if edits.replay_ocr_onboarding {
            self.ocr_onboarding_visible = true;
        }
        if edits.open_sharing {
            self.app.request_sharing_settings();
        }
    }

    /// Draws the one-time text-recognition introduction.
    ///
    /// A short-lived sheet, gated on the GUI workflow actually existing, and
    /// never a second Settings surface: replaying it is a request raised by the
    /// ordinary Settings window's Text Recognition pane.
    fn show_ocr_onboarding(&mut self, ctx: &egui::Context) {
        if !self.ocr_onboarding_visible {
            return;
        }
        let viewport = egui::ViewportId::from_hash_of("scrozz-ocr-onboarding");
        let mut close = false;
        let mut completed = false;
        ctx.show_viewport_immediate(
            viewport,
            egui::ViewportBuilder::default()
                .with_title("Text recognition")
                .with_inner_size([760.0, 500.0])
                .with_min_inner_size([760.0, 500.0])
                .with_resizable(false),
            |ui, _class| {
                close = ui.ctx().input(|input| input.viewport().close_requested());
                egui::CentralPanel::default().show(ui, |ui| {
                    completed = self.ocr_onboarding.ui(ui).completed;
                });
            },
        );

        if close || completed {
            if let Err(error) = self.ocr_onboarding_memory.mark_seen() {
                tracing::warn!(%error, "OCR onboarding completion could not be saved");
            }
            self.ocr_onboarding_visible = false;
            if completed {
                ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Close);
            }
        }
    }

    fn show_camera_settings(&mut self, ctx: &egui::Context) {
        let queued: Vec<_> = self
            .camera_settings_actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect();
        for action in queued {
            self.app.handle_camera_settings_action(action);
        }

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

const CARD_READY_OBSERVATIONS: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq)]
struct CardSurfaceTarget {
    geometry: RecentCapturesOverlayGeometry,
    scale: Option<ScaleFactor>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ViewportMeasurement {
    origin: egui::Pos2,
    size: egui::Vec2,
    native_scale: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CardArm {
    target: CardSurfaceTarget,
    ready_observations: u8,
    observed_scale: Option<f32>,
}

impl CardArm {
    const fn new(target: CardSurfaceTarget) -> Self {
        Self {
            target,
            ready_observations: 0,
            observed_scale: None,
        }
    }

    fn observe(&mut self, measurement: Option<ViewportMeasurement>) -> bool {
        let Some(measurement) = measurement else {
            self.observed_scale = None;
            return self.observe_ready(false);
        };
        if !viewport_matches(self.target, measurement) {
            self.observed_scale = None;
            return self.observe_ready(false);
        }

        if self.target.scale.is_none() {
            if self
                .observed_scale
                .is_some_and(|scale| (scale - measurement.native_scale).abs() > 0.01)
            {
                self.ready_observations = 0;
            }
            self.observed_scale = Some(measurement.native_scale);
        }
        self.observe_ready(true)
    }

    fn observe_ready(&mut self, ready: bool) -> bool {
        if ready {
            self.ready_observations = self.ready_observations.saturating_add(1);
        } else {
            self.ready_observations = 0;
        }
        self.ready_observations >= CARD_READY_OBSERVATIONS
    }

    fn resolved_target(self) -> CardSurfaceTarget {
        CardSurfaceTarget {
            scale: self.target.scale.or_else(|| {
                self.observed_scale
                    .filter(|scale| scale.is_finite() && *scale > 0.0)
                    .map(|scale| ScaleFactor::new(f64::from(scale)))
            }),
            ..self.target
        }
    }
}

fn root_viewport_measurement(ctx: &egui::Context) -> Option<ViewportMeasurement> {
    let zoom = ctx.zoom_factor();
    ctx.input(|input| {
        let viewport = input.viewport();
        let outer = viewport.outer_rect?;
        let inner = viewport.inner_rect?;
        Some(ViewportMeasurement {
            origin: egui::pos2(outer.min.x * zoom, outer.min.y * zoom),
            size: inner.size() * zoom,
            native_scale: viewport.native_pixels_per_point?,
        })
    })
}

fn viewport_matches(target: CardSurfaceTarget, measurement: ViewportMeasurement) -> bool {
    let scale = target
        .scale
        .map_or(measurement.native_scale, |scale| scale.get() as f32);
    let logical_tolerance = (1.0 / scale).max(0.25);
    let close = |left: f32, right: f32| (left - right).abs() <= logical_tolerance;
    let scale_matches = target
        .scale
        .is_none_or(|scale| (measurement.native_scale - scale.get() as f32).abs() <= 0.01);
    let geometry = target.geometry;
    scale_matches
        && close(measurement.origin.x, geometry.position().x)
        && close(measurement.origin.y, geometry.position().y)
        && close(measurement.size.x, geometry.size().x)
        && close(measurement.size.y, geometry.size().y)
}

fn shown_card_surface_matches(
    desired: CardSurfaceTarget,
    shown: CardSurfaceTarget,
    measurement: Option<ViewportMeasurement>,
) -> bool {
    if desired.geometry != shown.geometry {
        return false;
    }
    if let Some(scale) = desired.scale
        && shown.scale != Some(scale)
    {
        return false;
    }
    measurement.map_or(
        cfg!(target_os = "linux") && shown.scale.is_none(),
        |measurement| viewport_matches(shown, measurement),
    )
}

fn reveal_card_surface(
    native: &BehaviorController,
    ctx: &egui::Context,
    target: CardSurfaceTarget,
) {
    native.apply(&scrozz_shell::OverlayBehavior::capture_card());
    native.set_cursor(scrozz_shell::OverlayCursor::Arrow);
    native.set_frame(logical_frame(target.geometry));
    ctx.send_viewport_cmd(egui::ViewportCommand::ContentProtected(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));
    native.set_visible(true);
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootSurfaceMode {
    Hidden,
    Parked,
    ArmingCards,
    Cards,
    Selector,
}

const fn parked_root_requires_bootstrap(previous: RootSurfaceMode) -> bool {
    matches!(previous, RootSurfaceMode::Hidden)
}

fn visible_surface_refresh_due(previous: RootSurfaceMode, next: RootSurfaceMode) -> bool {
    matches!(next, RootSurfaceMode::Cards | RootSurfaceMode::Selector)
        && previous != next
        && !(previous == RootSurfaceMode::ArmingCards && next == RootSurfaceMode::Cards)
}

const fn display_refresh_requested(
    native_or_fallback: bool,
    visible_surface_transition: bool,
    pending_card: bool,
) -> bool {
    native_or_fallback || visible_surface_transition || pending_card
}

const fn live_surface_needs_display_fallback(
    mode: RootSurfaceMode,
    follow_active_display: bool,
) -> bool {
    follow_active_display && matches!(mode, RootSurfaceMode::Cards)
}

const fn root_surface_mode(
    selector_visible: bool,
    cards_allowed: bool,
    card_content: bool,
    auxiliary_open: bool,
) -> RootSurfaceMode {
    if selector_visible {
        RootSurfaceMode::Selector
    } else if cards_allowed && card_content {
        RootSurfaceMode::Cards
    } else if auxiliary_open {
        RootSurfaceMode::Parked
    } else {
        RootSurfaceMode::Hidden
    }
}

/// The ordinary recording-editor viewport, and its native activation state.
///
/// Kept beside the deferred pin viewports rather than inside `scrozz-ui` for
/// one reason: making the window genuinely *ordinary* — key, main, opaque,
/// mouse-accepting, normal level — is an AppKit call, and `scrozz-ui` forbids
/// unsafe code and does not depend on `scrozz-shell`. The window is created
/// here so the activation that follows it can be, too.
#[derive(Default)]
struct VideoEditorWindow {
    open: bool,
    /// Activation is retried until AppKit reports the window exists, because
    /// eframe creates a deferred viewport a frame or more after it is asked to.
    activation_pending: bool,
    activation_reported: bool,
}

impl VideoEditorWindow {
    const fn is_open(&self) -> bool {
        self.open
    }

    fn open(&mut self) {
        if !self.open {
            self.open = true;
            self.activation_pending = true;
            self.activation_reported = false;
        }
    }

    fn close(&mut self) {
        self.open = false;
        self.activation_pending = false;
    }
}

impl Driver {
    /// Draws the recording editor in its own opaque, focus-taking window.
    ///
    /// A deferred viewport, like history and unlike the annotation editor: the
    /// editor's preview worker publishes frames from another thread, and an
    /// immediate viewport would tie its repaint rate to the root overlay's.
    /// The root parks itself while this is open — see
    /// [`Driver::sync_root_visibility`] — so the transparent, click-through
    /// capture surface can never sit over a window the user is typing into.
    fn show_video_editor(&mut self, ctx: &egui::Context) {
        let actions: Vec<_> = self
            .video_editor_actions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect();
        for action in actions {
            self.app.handle_video_editor_action(action);
        }

        let snapshot = self.app.video_editor_snapshot();
        if snapshot.is_some() {
            self.video_editor.open();
        } else {
            self.video_editor.close();
            return;
        }
        let Some(snapshot) = snapshot else {
            return;
        };

        let viewport = scrozz_ui::video_editor_viewport_id();
        let theme = scrozz_ui::Theme::for_appearance(match ctx.theme() {
            egui::Theme::Dark => scrozz_ui::Appearance::Dark,
            egui::Theme::Light => scrozz_ui::Appearance::Light,
        });
        let root_context = ctx.clone();
        let collected = Arc::clone(&self.video_editor_actions);

        ctx.request_repaint_of(viewport);
        ctx.show_viewport_deferred(
            viewport,
            scrozz_ui::video_editor_viewport_builder()
                .with_active(true)
                .with_visible(true),
            move |ui, _class| {
                let close_requested = ui.ctx().input(|input| input.viewport().close_requested());
                let response = scrozz_ui::show_video_editor_window(ui, &snapshot, &theme);
                let mut queued = collected
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if close_requested {
                    queued.push(scrozz_ui::VideoEditorAction::Close);
                } else {
                    queued.extend(response.actions);
                }
                if !queued.is_empty() {
                    root_context.request_repaint_of(egui::ViewportId::ROOT);
                }
            },
        );
        self.activate_video_editor();
    }

    /// Makes the recording editor a genuinely ordinary key window.
    ///
    /// Retried rather than assumed: eframe creates the deferred viewport some
    /// frames after it is requested, and a failure to find it yet is ordinary
    /// latency, not an error worth telling anyone about.
    fn activate_video_editor(&mut self) {
        if !self.video_editor.activation_pending {
            return;
        }
        #[cfg(target_os = "macos")]
        {
            match scrozz_shell::macos::editor::activate(scrozz_ui::VIDEO_EDITOR_WINDOW_TITLE) {
                Ok(Some(diagnostics)) => {
                    self.video_editor.activation_pending = false;
                    tracing::debug!(?diagnostics, "recording editor became an ordinary window");
                }
                Ok(None) => {}
                Err(error) => {
                    self.video_editor.activation_pending = false;
                    if !self.video_editor.activation_reported {
                        self.video_editor.activation_reported = true;
                        tracing::warn!(%error, "recording editor could not be foregrounded");
                    }
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            // Windows and Linux get an ordinary decorated viewport from eframe
            // with no extra activation call; there is nothing honest to add.
            self.video_editor.activation_pending = false;
        }
    }

    fn show_history(&self, ctx: &egui::Context) {
        let history = self.app.history();
        let (visible, focus) = {
            let mut history = history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (history.is_visible(), history.take_focus_request())
        };
        if !visible {
            return;
        }

        let viewport = viewport_id();
        let waker = self.handle.clone();
        ctx.request_repaint_of(viewport);
        ctx.show_viewport_deferred(viewport, viewport_builder(focus), move |ui, _class| {
            let close_requested = ui.ctx().input(|input| input.viewport().close_requested());
            let mut history = history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if close_requested {
                history.close();
                return;
            }
            history.ui(ui);
            let needs_dispatch = history.has_pending_actions();
            drop(history);
            if needs_dispatch {
                waker.wake();
            }
        });
    }

    fn service_history_drag(&mut self, ctx: &egui::Context) {
        for session in &self.retired_drags {
            session.sweep();
        }
        self.retired_drags.retain(|session| !session.is_settled());

        let outcome = self
            .active_drag
            .as_ref()
            .and_then(|active| active.session.outcome());
        if let (Some(outcome), Some(active)) = (outcome, self.active_drag.take()) {
            self.app.history_drag_finished(&active.subject, &outcome);
            crate::gui::selection::release_modal_drag_input_for(ctx, viewport_id());
            active.session.sweep();
            if !active.session.is_settled() {
                self.retired_drags.push(active.session);
            }
        }
        if self.active_drag.is_some() {
            return;
        }

        let Some(pending) = self.app.take_history_drag() else {
            return;
        };
        let subject = pending.subject.clone();
        let Some(source) = self.drag_source.as_ref() else {
            self.app.history_drag_finished(
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
                self.app.history_drag_finished(
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
                self.app.history_drag_finished(
                    &subject,
                    &scrozz_shell::DragOutcome::Failed(error.to_string()),
                );
            }
        }
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
    /// # Native teardown ordering
    ///
    /// Winit owns the window class, delegate, and KVO registrations.
    /// [`Self::on_exit`] stops native mutation, orders utility surfaces out, and
    /// releases Scrozz's retains before eframe drops its viewport map.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "macos")]
        if self.automatic_termination.is_none()
            && let Some(completed) = self.appkit_bootstrap_frame_completed_at
        {
            let remaining = APPKIT_BOOTSTRAP_SETTLE.saturating_sub(completed.elapsed());
            if !remaining.is_zero() {
                ctx.request_repaint_after(remaining);
                return;
            }
            if let Tick::Stop(reason) = self.acquire_automatic_termination_after_bootstrap() {
                tracing::error!(
                    exit_reason = reason.label(),
                    "closing the root viewport after native lifecycle failure"
                );
                self.finish_shutdown();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }
        self.announce_panel();
        if self.app.take_sharing_settings_request() {
            self.cloud_settings.open(self.app.cloud_settings());
        }
        self.overlay.logic(ctx);
        if std::mem::take(&mut self.permission_resume_armed) {
            self.app.dispatch_permission_resume();
        }
        let selector_owned_the_window = self.selection.owns_surface();
        self.selection.logic(ctx, &self.native);
        let selector_owns_the_window = self.selection.owns_surface();
        if selector_owned_the_window || selector_owns_the_window {
            self.overlay.invalidate_passthrough_cache();
        }
        #[cfg(not(target_os = "macos"))]
        let disconnected_display_check_due = {
            let follow_active_display = self
                .app
                .recent_captures_overlay_settings()
                .follow_active_display;
            live_surface_needs_display_fallback(self.root_mode, follow_active_display)
                && Instant::now() >= self.next_active_display_refresh
        };
        #[cfg(target_os = "macos")]
        let disconnected_display_check_due = false;
        #[cfg(not(target_os = "macos"))]
        if disconnected_display_check_due {
            self.next_active_display_refresh = Instant::now() + DISPLAY_REFRESH;
        }
        let display_refresh_requested =
            self.native_display_parameters_changed() || disconnected_display_check_due;

        let editor = self
            .editing
            .as_ref()
            .map(|editing| EditorSnapshot::new(editing.card, editing.generation, &editing.editor));
        let tick = self.app.tick_with_editor(editor);
        if self.app.has_pending_save_dialog() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        if self.app.take_modal_drag_input_release() {
            crate::gui::selection::release_modal_drag_input(ctx);
        }
        if self.app.take_settings_request() {
            self.settings.open();
            #[cfg(target_os = "macos")]
            self.record_lifecycle("settings-requested");
        }
        if self.editing.is_none()
            && let Some(request) = self.app.take_editor_request()
        {
            if let Err(error) = self.color_picker.close() {
                tracing::warn!(%error, "the system colour picker could not close");
            }
            self.color_picker_generation = None;
            let title = format!("{}", request.card);
            let mut editor = scrozz_ui::editor::EditorUi::new(request.document);
            editor.set_custom_swatches(self.custom_swatches.clone());
            editor
                .state_mut()
                .set_custom_presets(request.smart_frame_presets);
            self.editing = Some(Editing {
                card: request.card,
                generation: request.generation,
                editor,
            });
            let _ = self.editor.open(title);
        }
        while let Some(result) = self.app.take_smart_frame_result() {
            let Some(editing) = self.editing.as_mut() else {
                continue;
            };
            if editing.card == result.card && editing.generation == result.generation {
                editing
                    .editor
                    .deliver_analysis(result.revision, result.result);
            }
        }
        self.sync_root_visibility(ctx, display_refresh_requested);
        if !self.stopped
            && let Tick::Stop(reason) = tick
        {
            tracing::info!(exit_reason = reason.label(), "closing the root viewport");
            self.overlay.flush_pin_states(ctx);
            #[cfg(target_os = "macos")]
            self.record_lifecycle("root-close-requested");
            self.finish_shutdown();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        #[cfg(not(target_os = "macos"))]
        if live_surface_needs_display_fallback(
            self.root_mode,
            self.app
                .recent_captures_overlay_settings()
                .follow_active_display,
        ) {
            ctx.request_repaint_after(DISPLAY_REFRESH);
        }

        if let Some(remaining) = self.app.remaining_deadline() {
            ctx.request_repaint_after(remaining);
        }
        // A recording is the one thing that keeps working while nothing is on
        // screen: its clock, its warnings and its finalisation all advance from
        // `App::tick`. The idle fallback alone would leave a stop request
        // waiting a quarter of a second, and a finished recording that long
        // without its card.
        if self.app.recording_is_busy() {
            ctx.request_repaint_after(RECORDING_TICK);
        }
        if self.app.video_editor_is_open() || self.video_editor.is_open() {
            ctx.request_repaint_after(VIDEO_EDITOR_TICK);
        }
        #[cfg(not(target_os = "macos"))]
        if matches!(
            self.root_mode,
            RootSurfaceMode::Hidden | RootSurfaceMode::Parked
        ) {
            // Tray, hotkey, pipeline, and IPC producers all request an immediate
            // repaint through the overlay handle. This slow wake is the
            // fail-safe: a platform event source must never leave a hidden
            // menu-bar app permanently asleep.
            ctx.request_repaint_after(IDLE_FALLBACK_WAKE);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        #[cfg(target_os = "macos")]
        scrozz_shell::macos::activity::record_root_redraw();
        #[cfg(target_os = "macos")]
        if self.parked_root_bootstrap_pending {
            self.parked_root_bootstrap_pending = false;
            self.record_lifecycle("auxiliary-root-bootstrap-committed");
            // Immediate child viewports must be registered on every parent UI
            // pass. Preserve already-open pins while delaying only the new
            // auxiliary child until the root's visible bootstrap is committed.
            self.show_capture_surface(ui, frame);
            self.reconcile_pin_panels(ui.ctx());
            ui.ctx().request_repaint();
            return;
        }
        let permission = self.app.permission_prompt();
        if let Some(response) = self.permission.show(ui.ctx(), permission.as_ref()) {
            if response == scrozz_ui::permission::PermissionResponse::UseApplePicker {
                self.permission.close(ui.ctx());
            }
            self.app.respond_to_permission(response);
        }
        if self.app.has_permission_resume() {
            self.permission.close(ui.ctx());
            self.permission_resume_armed = true;
        }

        self.show_settings(ui.ctx());
        self.show_ocr_onboarding(ui.ctx());
        self.show_cloud_settings(ui.ctx());
        self.show_editor(ui.ctx());
        self.show_video_editor(ui.ctx());
        self.show_camera_settings(ui.ctx());
        self.show_history(ui.ctx());
        self.service_history_drag(ui.ctx());
        self.show_capture_surface(ui, frame);
        self.reconcile_pin_panels(ui.ctx());
        #[cfg(target_os = "macos")]
        self.order_out_parked_root_after_auxiliary_registration(ui.ctx());

        #[cfg(target_os = "macos")]
        if self.appkit_bootstrap_frame_completed_at.is_none() {
            self.appkit_bootstrap_frame_completed_at = Some(Instant::now());
            self.record_lifecycle("appkit-bootstrap-frame-completed");
            ui.ctx().request_repaint_after(APPKIT_BOOTSTRAP_SETTLE);
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        // Forwarded, not defaulted: the overlay's clear colour is transparent,
        // and eframe's default is a dark wash that would paint a grey sheet
        // over the user's whole work area — the "stray window" failure exactly.
        self.overlay.clear_color(visuals)
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.finish_shutdown();
        self.prepare_native_teardown();
        #[cfg(target_os = "macos")]
        if let Some(mut guard) = self.automatic_termination.take() {
            guard.release();
        }
        #[cfg(target_os = "macos")]
        self.record_lifecycle("on-exit-complete");
        #[cfg(target_os = "macos")]
        {
            let activity =
                scrozz_shell::macos::activity::snapshot().since(self.native_activity_start);
            tracing::info!(
                screen_preflights = activity.screen_preflights,
                screen_requests = activity.screen_requests,
                display_enumerations = activity.display_enumerations,
                pointer_samples = activity.pointer_samples,
                root_redraws = activity.root_redraws,
                automatic_termination_disables = activity.automatic_termination_disables,
                automatic_termination_enables = activity.automatic_termination_enables,
                "native lifecycle activity"
            );
        }
    }
}

fn drag_origin(ctx: &egui::Context, pending: &PendingDrag) -> scrozz_core::Result<DragOrigin> {
    let mut geometry = pending.geometry;
    if !matches!(pending.subject, DragSubject::History(_)) {
        return Err(CoreError::InvalidRequest(
            "live card drags must use the overlay's same-frame drag path".into(),
        ));
    }
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
    let surface = native_surface_for_window(HISTORY_WINDOW_TITLE)?;
    Ok(DragOrigin::new(surface, geometry.rect, geometry.pointer))
}

fn geometry_in_viewport(mut geometry: DragGeometry, origin: egui::Pos2) -> DragGeometry {
    geometry.rect.origin.x -= f64::from(origin.x);
    geometry.rect.origin.y -= f64::from(origin.y);
    geometry.pointer.x -= f64::from(origin.x);
    geometry.pointer.y -= f64::from(origin.y);
    geometry
}

/// Where the overlay window goes.
///
/// The card layout uses the work area, not the display bounds: anchoring a card
/// to the bottom-left of the *bounds* puts it behind the Dock. The transparent
/// viewport may extend below that safe area solely so shadows can fade naturally.
fn work_area() -> CliResult<(
    RecentCapturesOverlayGeometry,
    Option<DisplayId>,
    Option<ScaleFactor>,
)> {
    #[cfg(target_os = "macos")]
    {
        let display = scrozz_shell::macos::display::active_display().map_err(CliError::from)?;
        let id = display.id.clone();
        Ok((
            geometry_from_display(&display)?,
            Some(id),
            Some(display.scale),
        ))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let backend = crate::platform::display_topology()?;
        let active = backend.active_display().ok();
        let displays = backend.displays()?;
        let display = active
            .as_ref()
            .and_then(|active| displays.iter().find(|display| display.id == active.id))
            .or_else(|| displays.iter().find(|display| display.is_primary))
            .or_else(|| displays.first())
            .ok_or_else(|| {
                CliError::Core(CoreError::Unsupported {
                    what: "opening the capture-card overlay".into(),
                    why: "the platform reported no native display work area".into(),
                })
            })?;
        let id = display.id.clone();
        Ok((
            geometry_from_display(display)?,
            Some(id),
            Some(display.scale),
        ))
    }
}

fn geometry_from_display(display: &Display) -> CliResult<RecentCapturesOverlayGeometry> {
    let area = display.work_area;
    if !area.origin.x.is_finite()
        || !area.origin.y.is_finite()
        || !area.size.width.is_finite()
        || !area.size.height.is_finite()
        || area.size.is_empty()
    {
        return Err(CliError::Core(CoreError::Platform(format!(
            "display {} reported an invalid native work area",
            display.id.0
        ))));
    }
    Ok(geometry_for_display(display))
}

fn pin_displays(_geometry: RecentCapturesOverlayGeometry) -> (DisplaySet, Option<DisplayId>) {
    if let Some(topology) = query_pin_topology() {
        return (topology.displays, topology.active_display);
    }

    tracing::warn!(
        "native display metrics are unavailable; Pin to Screen is disabled rather than using fabricated geometry"
    );
    (DisplaySet::new(Vec::new()), None)
}

fn query_pin_topology() -> Option<PinTopology> {
    #[cfg(target_os = "macos")]
    {
        let displays = scrozz_shell::macos::display::displays().ok()?;
        let active_display = scrozz_shell::macos::display::pointer_location()
            .ok()
            .and_then(|point| scrozz_shell::macos::display::display_for_point(&displays, point))
            .map(|display| display.id.clone());
        let displays = DisplaySet::new(displays);
        if displays.displays().is_empty() {
            return None;
        }
        let support = pin_support(&displays);
        Some(PinTopology {
            displays,
            active_display,
            support,
        })
    }

    #[cfg(not(target_os = "macos"))]
    let backend = match crate::platform::display_topology() {
        Ok(backend) => backend,
        Err(error) => {
            tracing::warn!("native pin display topology is unavailable: {error}");
            return None;
        }
    };
    #[cfg(not(target_os = "macos"))]
    let active_display = backend.active_display().ok().map(|display| display.id);
    #[cfg(not(target_os = "macos"))]
    let displays = DisplaySet::new(backend.displays().ok()?);
    #[cfg(not(target_os = "macos"))]
    if displays.displays().is_empty() {
        return None;
    }
    #[cfg(not(target_os = "macos"))]
    let support = pin_support(&displays);
    #[cfg(not(target_os = "macos"))]
    Some(PinTopology {
        displays,
        active_display,
        support,
    })
}

fn pin_support(displays: &DisplaySet) -> PinSupport {
    pin_support_from(scrozz_shell::pin::PinCapabilities::detect(), displays)
}

fn pin_support_from(
    capabilities: scrozz_shell::pin::PinCapabilities,
    displays: &DisplaySet,
) -> PinSupport {
    use scrozz_shell::pin::{PinBackend, Support};

    let mixed_dpi_windows = capabilities.backend == PinBackend::WindowsToolWindow
        && displays.displays().first().is_some_and(|first| {
            displays.displays()[1..]
                .iter()
                .any(|display| (display.scale.get() - first.scale.get()).abs() > f64::EPSILON)
        });
    let has_native_geometry = !displays.displays().is_empty();
    let positioning =
        capabilities.positioning.available() && !mixed_dpi_windows && has_native_geometry;
    let mut gaps = Vec::new();
    append_support_gap("global positioning", &capabilities.positioning, &mut gaps);
    append_support_gap("always on top", &capabilities.always_on_top, &mut gaps);
    append_support_gap(
        "non-activating input",
        &capabilities.non_activating,
        &mut gaps,
    );
    if mixed_dpi_windows {
        gaps.push(
            "global positioning: mixed-DPI Windows desktops need a native Win32 coordinate \
             adapter; Windows will place this pin until that adapter lands, or matching display \
             scales can be used"
                .into(),
        );
    }
    if !has_native_geometry {
        gaps.push(
            "display geometry: the platform did not report native monitor metrics; Pin to Screen is disabled"
                .into(),
        );
    }
    let detail = if gaps.is_empty() {
        format!("{:?} pinned windows", capabilities.backend)
    } else {
        gaps.join("; ")
    };
    PinSupport {
        windows: capabilities.pin_window.available() && has_native_geometry,
        positioning,
        always_on_top: capabilities.always_on_top.available(),
        native_opacity: matches!(capabilities.native_opacity, Support::Yes),
        click_through: capabilities.click_through.available(),
        non_activating: matches!(capabilities.non_activating, Support::Yes),
        native_adoption: matches!(
            capabilities.backend,
            PinBackend::MacWindow | PinBackend::WindowsToolWindow | PinBackend::X11ManagedDock
        ),
        x11_managed_dock: matches!(capabilities.backend, PinBackend::X11ManagedDock),
        detail,
    }
}

fn append_support_gap(label: &str, support: &scrozz_shell::pin::Support, gaps: &mut Vec<String>) {
    if let scrozz_shell::pin::Support::No { why, remedy } = support {
        gaps.push(format!("{label}: {why}; {remedy}"));
    }
}

/// An exact pointer source for the click-through logic, if one is available.
///
fn pointer_probe(geometry: SharedGeometry) -> Option<scrozz_ui::PointerProbe> {
    Some(Arc::new(move || {
        let current = *geometry.lock().ok()?;
        scrozz_shell::pointer_location()
            .ok()
            .map(|point| local_pointer(current, point))
    }))
}

fn local_pointer(
    geometry: RecentCapturesOverlayGeometry,
    point: scrozz_core::LogicalPoint,
) -> egui::Pos2 {
    let origin = geometry.position();
    egui::pos2(point.x as f32 - origin.x, point.y as f32 - origin.y)
}

#[cfg(not(target_os = "macos"))]
fn refreshed_work_area(
    current: RecentCapturesOverlayGeometry,
    current_scale: Option<ScaleFactor>,
    display_id: &mut Option<DisplayId>,
    follow_active_display: bool,
) -> (RecentCapturesOverlayGeometry, Option<ScaleFactor>) {
    if let Ok(backend) = crate::platform::display_topology()
        && let Ok(displays) = backend.displays()
    {
        let active = follow_active_display
            .then(|| backend.active_display().ok())
            .flatten();
        let display = select_recent_captures_display(
            &displays,
            display_id.as_ref(),
            active.as_ref(),
            follow_active_display,
        );
        if let Some(display) = display
            && let Ok(geometry) = geometry_from_display(display)
        {
            *display_id = Some(display.id.clone());
            return (geometry, Some(display.scale));
        }
    }

    (current, current_scale)
}

fn display_by_id<'a>(displays: &'a [Display], id: &DisplayId) -> Option<&'a Display> {
    displays.iter().find(|display| display.id == *id)
}

fn select_recent_captures_display<'a>(
    displays: &'a [Display],
    remembered: Option<&DisplayId>,
    active: Option<&'a Display>,
    follow_active_display: bool,
) -> Option<&'a Display> {
    let active = if follow_active_display {
        active.and_then(|display| display_by_id(displays, &display.id))
    } else {
        None
    };
    active
        .or_else(|| remembered.and_then(|id| display_by_id(displays, id)))
        .or_else(|| displays.iter().find(|display| display.is_primary))
        .or_else(|| displays.first())
}

fn geometry_for_display(display: &Display) -> RecentCapturesOverlayGeometry {
    let area = display.work_area;
    let work_area = egui::Rect::from_min_size(
        egui::pos2(area.origin.x as f32, area.origin.y as f32),
        egui::vec2(area.size.width as f32, area.size.height as f32),
    );
    let bounds_bottom = (display.bounds.origin.y + display.bounds.size.height) as f32;
    let viewport_bottom = (work_area.bottom() + card_gesture_envelope()).min(bounds_bottom);
    let viewport = egui::Rect::from_min_max(
        work_area.min,
        egui::pos2(work_area.right(), viewport_bottom),
    );
    RecentCapturesOverlayGeometry::with_viewport(work_area, viewport)
}

fn logical_frame(geometry: RecentCapturesOverlayGeometry) -> scrozz_core::LogicalRect {
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
    use crate::gui::panel::RecordedNativeAction;
    use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize, Provenance, ScaleFactor};

    fn card_target(scale: f64, origin: egui::Pos2) -> CardSurfaceTarget {
        let base = RecentCapturesOverlayGeometry::with_viewport(
            egui::Rect::from_min_size(origin, egui::vec2(1440.0, 850.0)),
            egui::Rect::from_min_size(origin, egui::vec2(1440.0, 1010.0)),
        );
        CardSurfaceTarget {
            geometry: card_surface_geometry(base, 1),
            scale: Some(ScaleFactor::new(scale)),
        }
    }

    fn exact_measurement(target: CardSurfaceTarget) -> ViewportMeasurement {
        ViewportMeasurement {
            origin: target.geometry.position(),
            size: target.geometry.size(),
            native_scale: target.scale.expect("test target scale").get() as f32,
        }
    }

    /// Which frame pass a call sits in, read from this file's own source.
    ///
    /// `Driver` needs a live `eframe::Frame` and a window, so no test can
    /// drive its two passes. But *which pass* starts a native drag is the
    /// whole bug — a drain moved from `ui` back into `logic` produces
    /// identical events one frame late, when the mouse button is already up
    /// and AppKit will refuse. That is exactly the kind of regression a
    /// reviewer would wave through, so it is pinned here rather than left to
    /// the comment beside it.
    fn driver_pass(name: &str) -> &'static str {
        let src = include_str!("host.rs");
        // The second `impl eframe::App` block in this file is `Driver`'s; the
        // first belongs to `OneShotDriver`.
        let driver = src
            .split("impl eframe::App for Driver")
            .nth(1)
            .expect("Driver must implement eframe::App");
        let logic = driver
            .split_once("fn logic(")
            .expect("Driver has a logic pass")
            .1;
        let (logic_body, after) = logic
            .split_once("fn ui(")
            .expect("the ui pass follows the logic pass");
        let ui_body = after
            .split_once("fn clear_color(")
            .expect("clear_color follows the ui pass")
            .0;

        match (logic_body.contains(name), ui_body.contains(name)) {
            (true, false) => "logic",
            (false, true) => "ui",
            (true, true) => "both",
            (false, false) => "neither",
        }
    }

    #[test]
    fn native_drags_are_started_in_the_ui_pass() {
        assert_eq!(
            driver_pass("self.show_capture_surface"),
            "ui",
            "native drags must begin inside the frame that drew the gesture, \
             while the mouse button is still down — not in the logic pass, \
             which runs a frame earlier and would act on a released button"
        );
        let source = include_str!("host.rs");
        let capture_surface = source
            .split("fn show_capture_surface")
            .nth(1)
            .and_then(|body| body.split_once("fn reconcile_pin_panels"))
            .map(|(body, _)| body)
            .expect("capture surface UI pass");
        assert!(capture_surface.contains("pump_drag_starts_with_editor"));
    }

    #[test]
    fn history_window_rendering_and_drag_start_stay_in_the_ui_pass() {
        assert_eq!(
            driver_pass("self.show_history"),
            "ui",
            "history must use an ordinary child viewport, not paint from hidden-root logic"
        );
        assert_eq!(
            driver_pass("self.service_history_drag"),
            "ui",
            "native history drag setup is main-thread UI work"
        );
    }

    #[test]
    fn history_actions_wake_root_logic_and_drag_artifacts_stay_sweepable() {
        let source = include_str!("host.rs");
        let history = source
            .split("fn show_history")
            .nth(1)
            .and_then(|body| body.split_once("fn service_history_drag"))
            .map(|(body, _)| body)
            .expect("history viewport host");
        assert!(history.contains("history.has_pending_actions()"));
        assert!(history.contains("waker.wake()"));

        let drag = source
            .split("fn service_history_drag")
            .nth(1)
            .and_then(|body| body.split_once("impl eframe::App for Driver"))
            .map(|(body, _)| body)
            .expect("history drag host");
        assert!(drag.contains("session.sweep()"));
        assert!(drag.contains("retired_drags"));
        assert!(drag.contains("release_modal_drag_input_for"));
    }

    #[test]
    fn permission_resume_closes_in_ui_and_dispatches_in_the_next_logic_pass() {
        assert_eq!(
            driver_pass("has_permission_resume"),
            "ui",
            "the permission viewport must close from the UI pass"
        );
        assert_eq!(
            driver_pass("dispatch_permission_resume"),
            "logic",
            "the granted action must wait until the following logic pass"
        );
    }

    #[test]
    fn editor_input_and_shortcuts_settle_before_the_drag_revision_is_chosen() {
        let src = include_str!("host.rs");
        let ui = src
            .split("impl eframe::App for Driver")
            .nth(1)
            .and_then(|driver| driver.split_once("fn ui("))
            .and_then(|(_, ui)| ui.split_once("fn clear_color("))
            .map(|(ui, _)| ui)
            .expect("Driver has a UI pass");
        let edits = ui
            .find("self.show_settings(ui.ctx())")
            .expect("settings and shortcut edits are applied");
        let editor = ui
            .find("self.show_editor(ui.ctx())")
            .expect("the editor is updated");
        let capture_surface_call = ui[editor..]
            .find("self.show_capture_surface(ui, frame)")
            .map(|offset| editor + offset)
            .expect("capture surface is drawn");
        let capture_surface = src
            .split("fn show_capture_surface")
            .nth(1)
            .and_then(|body| body.split_once("fn reconcile_pin_panels"))
            .map(|(body, _)| body)
            .expect("capture surface UI pass");
        let overlay = capture_surface
            .find("eframe::App::ui(&mut self.overlay, ui, frame)")
            .expect("cards are drawn");
        let drag = capture_surface
            .find("pump_drag_starts_with_editor")
            .expect("native drags are pumped");

        assert!(
            edits < editor && editor < capture_surface_call && overlay < drag,
            "shortcut ownership must settle before editor input, and that input must advance the \
             revision before an adjacent overlay draw and native drag start"
        );
    }

    #[test]
    fn dirty_editor_persistence_is_queued_before_its_cache_is_released() {
        let source = include_str!("host.rs");
        let editor = source
            .split("fn show_editor")
            .nth(1)
            .and_then(|body| body.split_once("fn announce_panel"))
            .map(|(body, _)| body)
            .expect("editor host");
        let persist = editor
            .find("persist_editor")
            .expect("dirty editor persistence");
        let release = editor.find("editor_closed").expect("editor cache release");
        assert!(persist < release);
    }

    #[test]
    fn the_ordinary_event_drain_stays_in_the_logic_pass() {
        // The other half. `tick` deliberately runs while the window is hidden,
        // so a menu-bar app still notices hotkeys at rest; moving it into `ui`
        // to "fix" drag timing would break that instead.
        assert_eq!(
            driver_pass("app.tick_with_editor"),
            "logic",
            "the app's ordinary tick must keep running in the logic pass"
        );
    }

    fn display(id: &str, x: f64, scale: f64) -> Display {
        let bounds = LogicalRect::new(
            LogicalPoint::new(x, 0.0),
            LogicalSize::new(1_920.0, 1_080.0),
        );
        Display {
            id: DisplayId(id.into()),
            name: id.into(),
            bounds,
            work_area: bounds,
            scale: ScaleFactor::new(scale),
            is_primary: id == "primary",
        }
    }

    #[test]
    fn a_headless_run_ends_by_itself() {
        // The property every automated run depends on. If this ever stops
        // holding, a test can leave a menu-bar item behind.
        let app = App::new(
            Config::sealed(),
            Box::new(Recording::new()),
            Arc::new(UnsupportedSelector::headless()),
            false,
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
    }

    #[test]
    fn mixed_dpi_windows_degrades_global_positioning_truthfully() {
        let session = scrozz_shell::Session {
            server: scrozz_shell::DisplayServer::Windows,
            compositor: scrozz_shell::Compositor::Other,
            desktop: String::new(),
        };
        let capabilities = scrozz_shell::pin::PinCapabilities::for_session(&session);
        let mixed = DisplaySet::new(vec![
            display("primary", 0.0, 1.0),
            display("retina", 1_920.0, 2.0),
        ]);
        let support = pin_support_from(capabilities.clone(), &mixed);
        assert!(!support.positioning);
        assert!(support.detail.contains("mixed-DPI Windows"));

        let uniform = DisplaySet::new(vec![
            display("primary", 0.0, 1.25),
            display("second", 1_920.0, 1.25),
        ]);
        assert!(pin_support_from(capabilities, &uniform).positioning);
        assert!(WINDOW_GAP.contains("SCROZZ_GUI_HEADLESS"));
    }

    #[test]
    fn missing_native_geometry_disables_pins_instead_of_inventing_a_display() {
        let session = scrozz_shell::Session {
            server: scrozz_shell::DisplayServer::Windows,
            compositor: scrozz_shell::Compositor::Other,
            desktop: String::new(),
        };
        let support = pin_support_from(
            scrozz_shell::pin::PinCapabilities::for_session(&session),
            &DisplaySet::new(Vec::new()),
        );

        assert!(!support.windows);
        assert!(!support.positioning);
        assert!(support.detail.contains("native monitor metrics"));
    }

    #[test]
    fn overlay_geometry_requires_a_real_nonempty_native_work_area() {
        let valid = display("primary", -1_920.0, 2.0);
        let geometry = geometry_from_display(&valid).expect("native geometry");
        assert_eq!(geometry.work_area.min.x, -1_920.0);

        let mut invalid = valid;
        invalid.work_area.size = LogicalSize::new(0.0, 1_080.0);
        assert!(geometry_from_display(&invalid).is_err());
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
    fn root_surface_mode_is_content_and_lifecycle_bounded() {
        assert_eq!(
            root_surface_mode(false, true, false, false),
            RootSurfaceMode::Hidden
        );
        assert_eq!(
            root_surface_mode(true, false, false, false),
            RootSurfaceMode::Selector
        );
        assert_eq!(
            root_surface_mode(false, true, true, false),
            RootSurfaceMode::Cards
        );
        assert_eq!(
            root_surface_mode(false, false, true, false),
            RootSurfaceMode::Hidden,
            "cards stay hidden during capture even if old cards still exist"
        );
        assert_eq!(
            root_surface_mode(false, true, false, true),
            RootSurfaceMode::Parked,
            "an auxiliary child gets an off-screen parent without exposing it"
        );
    }

    #[test]
    fn idle_and_auxiliary_surfaces_never_schedule_display_enumeration() {
        for mode in [
            RootSurfaceMode::Hidden,
            RootSurfaceMode::Parked,
            RootSurfaceMode::Cards,
            RootSurfaceMode::Selector,
        ] {
            for _ in 0..240 {
                assert!(
                    !visible_surface_refresh_due(mode, mode),
                    "60 seconds of an unchanged {mode:?} surface must not poll displays"
                );
            }
        }
        assert!(visible_surface_refresh_due(
            RootSurfaceMode::Hidden,
            RootSurfaceMode::Cards
        ));
        assert!(visible_surface_refresh_due(
            RootSurfaceMode::Parked,
            RootSurfaceMode::Selector
        ));
        assert!(
            !visible_surface_refresh_due(RootSurfaceMode::ArmingCards, RootSurfaceMode::Cards),
            "the hidden framebuffer barrier is one capture transition, not a second query"
        );
        assert!(
            !live_surface_needs_display_fallback(RootSurfaceMode::Hidden, true),
            "an idle follow-active root must not poll"
        );
        assert!(
            !live_surface_needs_display_fallback(RootSurfaceMode::Parked, true),
            "Settings and editor frames must not poll"
        );
        assert!(
            live_surface_needs_display_fallback(RootSurfaceMode::Cards, true),
            "only a truly live non-macOS card surface may use the fallback"
        );

        let idle_refreshes = (0..240)
            .filter(|_| display_refresh_requested(false, false, false))
            .count();
        assert_eq!(
            idle_refreshes, 0,
            "60 seconds of idle/follow-active Settings frames must enumerate zero displays"
        );
    }

    #[test]
    fn display_events_and_visible_card_transitions_refresh_exactly_once() {
        assert_eq!(
            [true, false]
                .into_iter()
                .filter(|changed| display_refresh_requested(*changed, false, false))
                .count(),
            1,
            "one consumed display-change notification causes one refresh"
        );
        assert!(display_refresh_requested(
            false,
            visible_surface_refresh_due(RootSurfaceMode::Hidden, RootSurfaceMode::Cards),
            false
        ));
        assert!(display_refresh_requested(false, false, true));
        assert!(!display_refresh_requested(
            false,
            visible_surface_refresh_due(RootSurfaceMode::Cards, RootSurfaceMode::Cards),
            false,
        ));
        assert!(
            display_refresh_requested(true, true, true),
            "coincident native and visibility triggers collapse to one refresh decision"
        );
    }

    #[test]
    fn every_eframe_exit_returns_native_windows_before_viewport_drop() {
        let source = include_str!("host.rs");
        let driver = source
            .split("impl eframe::App for Driver")
            .nth(1)
            .expect("Driver must implement eframe::App");
        let on_exit = driver
            .split("fn on_exit")
            .nth(1)
            .expect("Driver must own eframe teardown")
            .split_once("\n    }")
            .map(|(body, _)| body)
            .expect("on_exit body");

        assert!(on_exit.contains("self.finish_shutdown()"));
        assert!(on_exit.contains("self.prepare_native_teardown()"));
        #[cfg(target_os = "macos")]
        {
            assert!(on_exit.contains("self.automatic_termination.take()"));
            assert!(
                on_exit.find("self.prepare_native_teardown()")
                    < on_exit.find("self.automatic_termination.take()"),
                "automatic termination must remain inhibited until native adapters are retired"
            );
        }
        let implementation = source
            .split("#[cfg(test)]")
            .next()
            .expect("implementation precedes tests");
        assert!(
            !implementation.contains("std::process::exit(0)"),
            "native teardown must complete instead of bypassing destructors"
        );
    }

    #[test]
    fn automatic_termination_is_inhibited_only_after_eframe_bootstrap() {
        let source = include_str!("host.rs");
        let run_native = source
            .split("eframe::run_native(")
            .next()
            .expect("host setup precedes eframe");
        assert!(
            !run_native.contains("AutomaticTerminationGuard::acquire"),
            "AppKit balances its bootstrap lease after run_native starts"
        );
        assert!(
            source.contains("automatic_termination: None"),
            "the Driver must begin without an early lease"
        );
        assert!(
            source.contains("appkit_bootstrap_frame_completed_at: None"),
            "the Driver must wait for its initial root frame"
        );

        let logic = source
            .split("impl eframe::App for Driver")
            .nth(1)
            .and_then(|driver| driver.split_once("fn logic(&mut self, ctx:"))
            .map(|(_, logic)| logic)
            .expect("Driver logic pass");
        let acquire = logic
            .find("self.acquire_automatic_termination_after_bootstrap()")
            .expect("post-bootstrap Driver logic acquires the app lease");
        let settle = logic
            .find("APPKIT_BOOTSTRAP_SETTLE.saturating_sub")
            .expect("Driver waits for AppKit to balance its bootstrap lease");
        let app_work = logic
            .find("self.announce_panel()")
            .expect("Driver app work follows lifecycle setup");
        assert!(
            settle < acquire && acquire < app_work,
            "the post-bootstrap lease must precede all normal Driver work"
        );
        let ui = logic
            .split_once("fn ui(&mut self, ui:")
            .map(|(_, ui)| ui)
            .expect("Driver UI pass");
        assert!(
            ui.contains("appkit_bootstrap_frame_completed_at = Some(Instant::now())")
                && ui.contains("request_repaint_after(APPKIT_BOOTSTRAP_SETTLE)"),
            "the first completed root frame must schedule exactly one post-bootstrap lease wake"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn lifecycle_diagnostic_persists_the_exact_exit_reason() {
        let root = std::env::temp_dir().join(format!(
            "scrozz-lifecycle-diagnostic-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let diagnostic = LifecycleDiagnostic {
            path: root.join("last-lifecycle.json"),
            temporary: root.join("last-lifecycle.tmp"),
            baseline: scrozz_shell::macos::activity::snapshot(),
        };
        diagnostic
            .persist(
                "shutdown-recorded",
                Some(crate::gui::app::ExitReason::NativeEventLoop),
                false,
                RootSurfaceMode::Parked,
            )
            .expect("diagnostic write");

        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&diagnostic.path).expect("durable diagnostic"))
                .expect("diagnostic JSON");
        assert_eq!(document["state"], "shutdown-recorded");
        assert_eq!(document["exit_reason"], "native-event-loop");
        assert_eq!(document["root_mode"], "Parked");
        std::fs::remove_dir_all(root).expect("diagnostic cleanup");
    }

    #[test]
    fn parked_root_bootstraps_only_from_an_invisible_surface() {
        assert!(parked_root_requires_bootstrap(RootSurfaceMode::Hidden));
        assert!(
            !parked_root_requires_bootstrap(RootSurfaceMode::ArmingCards),
            "ArmingCards can already own auxiliary child viewports that must remain registered"
        );
        assert!(!parked_root_requires_bootstrap(RootSurfaceMode::Cards));
        assert!(!parked_root_requires_bootstrap(RootSurfaceMode::Selector));
        assert!(!parked_root_requires_bootstrap(RootSurfaceMode::Parked));
    }

    #[test]
    fn auxiliary_children_park_the_root_without_controlling_card_input() {
        let source = include_str!("host.rs");
        let visibility = source
            .split("fn sync_root_visibility")
            .nth(1)
            .and_then(|body| body.split_once("fn show_settings"))
            .map(|(body, _)| body)
            .expect("root visibility synchronizer");

        assert!(visibility.contains("self.settings.is_open()"));
        assert!(visibility.contains("self.editor.is_open()"));
        assert!(visibility.contains("self.app.video_editor_is_open()"));
        assert!(visibility.contains("self.video_editor.is_open()"));
        assert!(visibility.contains("self.app.permission_prompt().is_some()"));
        assert!(visibility.contains(".is_visible()"));
        assert!(visibility.contains("self.sync_card_visibility(ctx)"));

        let parked = visibility
            .split("RootSurfaceMode::Parked =>")
            .nth(1)
            .and_then(|body| body.split_once("RootSurfaceMode::Hidden =>"))
            .map(|(body, _)| body)
            .expect("parked root branch");
        assert!(parked.contains("self.native.set_visible(true)"));
        assert!(parked.contains("ViewportCommand::Visible(true)"));
        assert!(parked.contains("parked_root_requires_bootstrap(self.root_mode)"));
        assert!(parked.contains("ctx.request_repaint()"));
        assert!(
            !parked.contains("ViewportCommand::Close"),
            "an auxiliary child bootstraps through the root but never destroys it"
        );

        let order_out = source
            .split("fn order_out_parked_root_after_auxiliary_registration")
            .nth(1)
            .and_then(|body| body.split_once("fn show_settings"))
            .map(|(body, _)| body)
            .expect("post-registration root order-out");
        assert!(order_out.contains("self.native.set_visible(false)"));
        assert!(order_out.contains("ViewportCommand::Visible(false)"));
        assert!(
            !order_out.contains("ViewportCommand::Close"),
            "the registered child keeps the event loop alive while the root is ordered out"
        );
        let ui = source
            .split("impl eframe::App for Driver")
            .nth(1)
            .expect("Driver implementation")
            .split("fn ui(&mut self, ui:")
            .nth(1)
            .expect("Driver UI pass");
        let barrier = ui
            .find("if self.parked_root_bootstrap_pending")
            .expect("parked-root bootstrap barrier");
        let barrier_end = ui[barrier..]
            .find("return;")
            .map(|offset| barrier + offset)
            .expect("bootstrap barrier return");
        let settings = ui
            .find("self.show_settings")
            .expect("Settings registration");
        let capture_surface = ui[settings..]
            .find("self.show_capture_surface")
            .map(|offset| settings + offset)
            .expect("capture child registration");
        let pin_reconcile = ui[capture_surface..]
            .find("self.reconcile_pin_panels")
            .map(|offset| capture_surface + offset)
            .expect("native pin reconciliation");
        let order_out = ui[pin_reconcile..]
            .find("self.order_out_parked_root_after_auxiliary_registration")
            .map(|offset| pin_reconcile + offset)
            .expect("root order-out");
        assert!(
            barrier < settings,
            "the root visibility command must commit before an immediate child is registered"
        );
        assert!(
            ui[barrier..barrier_end].contains("self.show_capture_surface")
                && ui[barrier..barrier_end].contains("self.reconcile_pin_panels"),
            "the bootstrap pass must preserve already-open pin child viewports"
        );
        assert!(
            settings < capture_surface
                && capture_surface < pin_reconcile
                && pin_reconcile < order_out,
            "the root may be ordered out only after every child viewport and native pin is registered"
        );
    }

    #[test]
    fn the_video_editor_is_an_ordinary_window_the_root_parks_behind() {
        // Three properties, and each has its own way of going wrong:
        //
        // 1. The viewport is opaque and not click-through, or the editor is a
        //    transparent hole the user types straight through.
        // 2. The root parks while it is open, or the click-through capture
        //    surface sits over a window being typed into.
        // 3. It is exactly 1180x820 and a normal window level, not a panel.
        let builder = scrozz_ui::video_editor_viewport_builder();
        assert_eq!(builder.transparent, Some(false));
        assert_eq!(builder.mouse_passthrough, Some(false));
        assert_eq!(builder.decorations, Some(true));
        assert_eq!(builder.window_level, Some(egui::WindowLevel::Normal));
        assert_eq!(builder.inner_size, Some(egui::vec2(1180.0, 820.0)));

        assert_eq!(
            root_surface_mode(false, true, false, true),
            RootSurfaceMode::Parked,
            "an open video editor parks the shared root rather than hiding it"
        );

        let source = include_str!("host.rs");
        let show = source
            .split("fn show_video_editor")
            .nth(1)
            .and_then(|body| body.split_once("fn activate_video_editor"))
            .map(|(body, _)| body)
            .expect("video editor viewport pass");
        assert!(
            show.contains("show_viewport_deferred"),
            "the editor is a deferred viewport so its preview worker drives its own repaints"
        );
        assert!(
            show.contains("handle_video_editor_action"),
            "every action the editor raised must reach the coordinator"
        );
        assert!(
            show.contains("request_repaint_of(egui::ViewportId::ROOT)"),
            "child input must wake the root pass that owns the editor coordinator"
        );
        assert!(
            show.contains("VideoEditorAction::Close"),
            "a native close request must close the editor rather than be swallowed"
        );
    }

    #[test]
    fn the_video_editor_window_only_arms_activation_on_a_real_open() {
        let mut window = VideoEditorWindow::default();
        assert!(!window.is_open());
        assert!(!window.activation_pending);

        window.open();
        assert!(window.is_open() && window.activation_pending);

        // Re-entering the same open editor must not re-steal the foreground on
        // every frame; only a fresh open arms activation again.
        window.activation_pending = false;
        window.open();
        assert!(!window.activation_pending);

        window.close();
        assert!(!window.is_open() && !window.activation_pending);
        window.open();
        assert!(window.activation_pending, "reopening arms activation once");
    }

    #[test]
    fn macos_hidden_root_has_no_periodic_idle_wake() {
        let source = include_str!("host.rs");
        let logic = source
            .split("impl eframe::App for Driver")
            .nth(1)
            .and_then(|driver| driver.split("fn logic(&mut self, ctx:").nth(1))
            .and_then(|body| body.split_once("\n    fn ui("))
            .map(|(body, _)| body)
            .expect("Driver logic pass");

        #[cfg(target_os = "macos")]
        assert!(
            logic.contains("#[cfg(not(target_os = \"macos\"))]\n        if matches!("),
            "macOS producers wake the root explicitly; idle frames must not poll AppKit"
        );
        #[cfg(not(target_os = "macos"))]
        {
            assert!(logic.contains("ctx.request_repaint_after(IDLE_FALLBACK_WAKE)"));
            assert!(IDLE_FALLBACK_WAKE >= Duration::from_millis(100));
        }
        assert!(logic.contains("recording_is_busy()"));
        assert!(logic.contains("RECORDING_TICK"));
    }

    #[test]
    fn first_card_waits_for_two_settled_native_measurements_at_every_scale() {
        for scale in [1.0, 1.25, 2.0] {
            let target = card_target(scale, egui::pos2(100.0, 40.0));
            let mut arm = CardArm::new(target);
            let parked = ViewportMeasurement {
                origin: egui::pos2(PARKED_ROOT_ORIGIN, PARKED_ROOT_ORIGIN),
                size: egui::vec2(1.0, 1.0),
                native_scale: 1.0,
            };

            assert!(
                !arm.observe(Some(parked)),
                "{scale}x must not reveal the parked framebuffer"
            );
            assert!(
                !arm.observe(Some(exact_measurement(target))),
                "{scale}x needs a complete hidden window-server turn"
            );
            assert!(
                arm.observe(Some(exact_measurement(target))),
                "{scale}x should reveal after two stable observations"
            );

            let slot = scrozz_ui::stack::StackLayout::new(
                target.geometry.local(),
                scrozz_ui::stack::CardMetrics::default(),
            )
            .slot_rect(0);
            let expected = egui::vec2(
                scrozz_ui::stack::CardMetrics::PREFERRED_WIDTH,
                scrozz_ui::stack::CardMetrics::PREFERRED_HEIGHT,
            );
            assert_eq!(slot.size(), expected);
            assert_eq!(slot.size() * scale as f32, expected * scale as f32);
        }
    }

    #[test]
    fn platforms_without_a_premeasured_display_scale_accept_the_native_scale() {
        let mut target = card_target(1.0, egui::pos2(40.0, 24.0));
        target.scale = None;
        let first_scale = exact_measurement(CardSurfaceTarget {
            scale: Some(ScaleFactor::new(1.25)),
            ..target
        });
        let second_scale = exact_measurement(CardSurfaceTarget {
            scale: Some(ScaleFactor::new(1.5)),
            ..target
        });

        let mut arm = CardArm::new(target);
        assert!(!arm.observe(Some(first_scale)));
        assert!(
            !arm.observe(Some(second_scale)),
            "a changing native scale must restart the stable observation count"
        );
        assert!(arm.observe(Some(second_scale)));
        let resolved = arm.resolved_target();
        assert_eq!(resolved.scale, Some(ScaleFactor::new(1.5)));
        assert!(shown_card_surface_matches(
            target,
            resolved,
            Some(second_scale)
        ));
        assert!(
            !shown_card_surface_matches(target, resolved, Some(first_scale)),
            "a visible non-mac card root must re-arm after a DPI transition"
        );
    }

    #[test]
    fn every_capture_kind_uses_the_same_full_size_first_card_barrier() {
        let target = card_target(2.0, egui::pos2(0.0, 33.0));
        for kind in crate::gui::action::CaptureKind::ALL {
            let mut arm = CardArm::new(target);
            assert!(
                !arm.observe(Some(exact_measurement(target))),
                "{} must not bypass first-card arming",
                kind.label()
            );
            assert!(
                arm.observe(Some(exact_measurement(target))),
                "{} did not reach the ordinary card geometry",
                kind.label()
            );

            let slot = scrozz_ui::stack::StackLayout::new(
                target.geometry.local(),
                scrozz_ui::stack::CardMetrics::default(),
            )
            .slot_rect(0);
            let provenance = match kind {
                crate::gui::action::CaptureKind::Window => Provenance::Window,
                crate::gui::action::CaptureKind::Region
                | crate::gui::action::CaptureKind::AllInOne => Provenance::Region,
                crate::gui::action::CaptureKind::Fullscreen => Provenance::Display,
                crate::gui::action::CaptureKind::AllDisplays => Provenance::AllDisplays,
                crate::gui::action::CaptureKind::Scrolling => Provenance::Window,
            };
            let preview = scrozz_ui::card::CardChrome::for_provenance(provenance)
                .geometry(slot, (3840, 2160));
            assert_eq!(preview.container, slot);
            assert_eq!(preview.capture, slot);
        }
    }

    #[test]
    fn every_empty_to_first_cycle_rearms_and_display_changes_invalidate_readiness() {
        let first = card_target(2.0, egui::pos2(0.0, 33.0));
        for _ in 0..3 {
            let mut arm = CardArm::new(first);
            assert!(!arm.observe(Some(exact_measurement(first))));
            assert!(arm.observe(Some(exact_measurement(first))));
        }

        let moved = card_target(1.25, egui::pos2(1440.0, 24.0));
        let mut arm = CardArm::new(moved);
        assert!(
            !arm.observe(Some(exact_measurement(first))),
            "old display geometry/scale must never unlock a moved card root"
        );
        assert!(!arm.observe(Some(exact_measurement(moved))));
        assert!(arm.observe(Some(exact_measurement(moved))));
    }

    #[test]
    fn card_input_and_tracking_are_installed_before_native_visibility() {
        let target = card_target(2.0, egui::pos2(0.0, 33.0));
        let ctx = egui::Context::default();
        let (native, _) = BehaviorController::recording();
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {
            reveal_card_surface(&native, &ctx, target);
        });
        output.textures_delta.clear();

        let actions = native.recorded_actions();
        let behavior = actions
            .iter()
            .position(|action| {
                *action
                    == RecordedNativeAction::Behavior(scrozz_shell::OverlayBehavior::capture_card())
            })
            .expect("capture-card behavior");
        let visible = actions
            .iter()
            .position(|action| *action == RecordedNativeAction::Visible(true))
            .expect("native orderFront");
        assert!(behavior < visible);
        assert!(
            !scrozz_shell::OverlayBehavior::capture_card().click_through,
            "the sole card root must own pointer tracking before it is shown"
        );

        let commands = &output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport")
            .commands;
        let input = commands
            .iter()
            .position(|command| matches!(command, egui::ViewportCommand::MousePassthrough(false)))
            .expect("winit input enable");
        let show = commands
            .iter()
            .position(|command| matches!(command, egui::ViewportCommand::Visible(true)))
            .expect("winit visibility");
        assert!(input < show);
    }

    #[test]
    fn app_host_parks_only_its_private_root_bootstrap() {
        let geometry = RecentCapturesOverlayGeometry::new(egui::Rect::from_min_size(
            egui::pos2(40.0, 30.0),
            egui::vec2(800.0, 600.0),
        ));
        let generic = scrozz_ui::recent_captures_overlay::native_options(geometry);
        let parked = parked_native_options(geometry);

        assert_eq!(generic.viewport.visible, None);
        assert_eq!(parked.viewport.visible, Some(false));
        if cfg!(target_os = "macos") {
            assert_eq!(
                parked.viewport.position,
                Some(egui::pos2(-100_000.0, -100_000.0))
            );
            assert_eq!(parked.viewport.inner_size, Some(egui::vec2(1.0, 1.0)));
        }
    }

    #[test]
    fn card_surface_is_bounded_to_the_occupied_stack_column() {
        let base = RecentCapturesOverlayGeometry::with_viewport(
            egui::Rect::from_min_size(egui::pos2(0.0, 33.0), egui::vec2(1728.0, 950.0)),
            egui::Rect::from_min_size(egui::pos2(0.0, 33.0), egui::vec2(1728.0, 1084.0)),
        );
        let one = card_surface_geometry(base, 1);
        let three = card_surface_geometry(base, 3);

        assert!(one.viewport().width() < 600.0);
        assert!(one.viewport().height() < 500.0);
        assert_eq!(one.viewport().width(), three.viewport().width());
        assert!(three.viewport().height() > one.viewport().height());
        assert!(three.viewport().height() < base.viewport().height());
        assert_eq!(one.work_area, base.work_area);
        assert_eq!(three.work_area, base.work_area);

        let layout = scrozz_ui::stack::StackLayout::new(
            base.local(),
            scrozz_ui::stack::CardMetrics::default(),
        );
        let resting = layout.slot_rect(0).translate(base.position().to_vec2());
        let gestures = scrozz_ui::stack::GestureConfig::default();
        assert!(
            one.viewport()
                .contains_rect(resting.translate(egui::vec2(gestures.dragout_dist, 0.0)))
        );
        assert!(
            one.viewport()
                .contains_rect(resting.translate(egui::vec2(0.0, gestures.collapse_dist)))
        );
    }

    #[test]
    fn compact_card_geometry_preserves_global_slot_positions() {
        let base = RecentCapturesOverlayGeometry::with_viewport(
            egui::Rect::from_min_size(egui::pos2(120.0, 85.0), egui::vec2(1200.0, 800.0)),
            egui::Rect::from_min_size(egui::pos2(120.0, 85.0), egui::vec2(1200.0, 848.0)),
        );
        let compact = card_surface_geometry(base, 2);
        let metrics = scrozz_ui::stack::CardMetrics::default();
        let base_slot = scrozz_ui::stack::StackLayout::new(base.local(), metrics).slot_rect(1);
        let compact_slot =
            scrozz_ui::stack::StackLayout::new(compact.local(), metrics).slot_rect(1);

        assert_eq!(
            base_slot.translate(base.position().to_vec2()),
            compact_slot.translate(compact.position().to_vec2())
        );
    }

    #[test]
    fn pointer_probe_is_available_on_macos() {
        let probe = pointer_probe(Arc::new(Mutex::new(
            RecentCapturesOverlayGeometry::default(),
        )));
        assert_eq!(probe.is_some(), cfg!(target_os = "macos"));
    }

    #[test]
    fn pointer_coordinates_are_relative_to_the_live_work_area() {
        let geometry = RecentCapturesOverlayGeometry::new(egui::Rect::from_min_size(
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
        let geometry = RecentCapturesOverlayGeometry::with_viewport(work_area, viewport);
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
        assert_eq!(geometry.viewport().bottom(), 1117.0);
        assert_eq!(geometry.work_area.bottom(), 983.0);
        assert_eq!(
            geometry.position().y + bottom_card.bottom(),
            981.0,
            "the card itself stays two points above the inferred Dock"
        );
        assert_eq!(
            geometry.size().y - bottom_card.bottom(),
            136.0,
            "the viewport preserves the downward gesture envelope within display bounds"
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
    fn recent_captures_display_policy_is_sticky_or_active_as_configured() {
        let display = |id: &str, primary: bool| Display {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds: LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(800.0, 600.0)),
            work_area: LogicalRect::new(
                LogicalPoint::new(0.0, 24.0),
                LogicalSize::new(800.0, 540.0),
            ),
            scale: ScaleFactor::new(1.0),
            is_primary: primary,
        };
        let displays = [display("primary", true), display("active", false)];
        let remembered = DisplayId("primary".to_owned());

        assert_eq!(
            select_recent_captures_display(
                &displays,
                Some(&remembered),
                Some(&displays[1]),
                false,
            )
            .map(|display| &display.id),
            Some(&displays[0].id)
        );
        assert_eq!(
            select_recent_captures_display(&displays, Some(&remembered), Some(&displays[1]), true,)
                .map(|display| &display.id),
            Some(&displays[1].id)
        );
    }

    #[test]
    fn recent_captures_display_removal_falls_back_to_primary_then_first() {
        let display = |id: &str, primary: bool| Display {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds: LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(800.0, 600.0)),
            work_area: LogicalRect::new(
                LogicalPoint::new(0.0, 24.0),
                LogicalSize::new(800.0, 540.0),
            ),
            scale: ScaleFactor::new(1.0),
            is_primary: primary,
        };
        let missing = DisplayId("disconnected".to_owned());
        let with_primary = [display("first", false), display("primary", true)];
        let disconnected_active = display("disconnected", false);
        assert_eq!(
            select_recent_captures_display(
                &with_primary,
                Some(&missing),
                Some(&disconnected_active),
                true,
            )
            .map(|display| &display.id),
            Some(&with_primary[1].id)
        );

        let without_primary = [display("first", false), display("second", false)];
        assert_eq!(
            select_recent_captures_display(&without_primary, Some(&missing), None, false)
                .map(|display| &display.id),
            Some(&without_primary[0].id)
        );
        assert!(
            select_recent_captures_display(&[], Some(&missing), None, false).is_none(),
            "no display must preserve the last known geometry"
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
